// Copyright (C) 2020 Jason Ish
//
// Permission is hereby granted, free of charge, to any person obtaining
// a copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to
// permit persons to whom the Software is furnished to do so, subject to
// the following conditions:
//
// The above copyright notice and this permission notice shall be
// included in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE
// LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION
// WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

use super::super::eventstore::EventStore;
use crate::elastic::{self, query_string_query, request::Request};
use crate::{
    datastore::{DatastoreError, EventQueryParams},
    types::JsonValue,
};

pub async fn dhcp_report(
    ds: &EventStore,
    what: &str,
    params: &EventQueryParams,
) -> Result<JsonValue, DatastoreError> {
    let mut filters = vec![elastic::request::term_filter(
        &ds.map_field("event_type"),
        "dhcp",
    )];

    if let Some(dt) = params.min_timestamp {
        filters.push(elastic::request::timestamp_gte_filter(dt));
    }

    if let Some(query_string) = &params.query_string {
        filters.push(query_string_query(query_string));
    }

    match what {
        "ack" => dhcp_report_ack(ds, filters).await,
        "request" => dhcp_report_request(ds, filters).await,
        "servers" => servers(ds, filters).await,
        "mac" => mac(ds, filters).await,
        "ip" => ip(ds, filters).await,
        _ => Err(anyhow::anyhow!("No DHCP report for {}", what).into()),
    }
}

pub async fn dhcp_report_ack(
    ds: &EventStore,
    mut filters: Vec<JsonValue>,
) -> Result<JsonValue, DatastoreError> {
    let mut request = elastic::request::new_request();
    filters.push(elastic::request::term_filter(
        &ds.map_field("dhcp.dhcp_type"),
        "ack",
    ));
    request.set_filters(filters);

    let aggs = json!({
        "client_mac": {
          "terms": {
            "field": ds.map_field("dhcp.client_mac"),
            "size": 10000
          },
          "aggs": {
            "latest": {
              "top_hits": {
                "sort": [
                  {
                    "@timestamp": {"order": "desc"}
                  }
                ],
                "size": 1
              }
            }
          }
        }
    });

    request["aggs"] = aggs;
    request.size(0);

    let response: JsonValue = ds.search(&request).await?.json().await?;

    let mut results = Vec::new();

    if let Some(buckets) = response["aggregations"]["client_mac"]["buckets"].as_array() {
        for bucket in buckets {
            let latest = &bucket["latest"]["hits"]["hits"][0]["_source"];
            let entry = map_dhcp_event(latest, ds.ecs);
            results.push(entry);
        }
    }

    Ok(json!({
        "data": results,
    }))
}

pub async fn dhcp_report_request(
    ds: &EventStore,
    mut filters: Vec<JsonValue>,
) -> Result<JsonValue, DatastoreError> {
    let mut request = elastic::request::new_request();
    filters.push(elastic::request::term_filter(
        &ds.map_field("dhcp.dhcp_type"),
        "request",
    ));
    request.set_filters(filters);

    let aggs = json!({
        "client_mac": {
          "terms": {
            "field": ds.map_field("dhcp.client_mac"),
            "size": 10000
          },
          "aggs": {
            "latest": {
              "top_hits": {
                "sort": [
                  {
                    "@timestamp": {
                      "order": "desc"
                    }
                  }
                ],
                "size": 1
              }
            }
          }
        }
    });

    request["aggs"] = aggs;
    request.size(0);

    let response: JsonValue = ds.search(&request).await?.json().await?;

    let mut results = Vec::new();

    if let Some(buckets) = response["aggregations"]["client_mac"]["buckets"].as_array() {
        for bucket in buckets {
            let latest = &bucket["latest"]["hits"]["hits"][0]["_source"];
            let entry = map_dhcp_event(latest, ds.ecs);
            results.push(entry);
        }
    }

    Ok(json!({
        "data": results,
    }))
}

/// Return all IP addresses that appear to be DHCP servers.
pub async fn servers(
    ds: &EventStore,
    mut filters: Vec<JsonValue>,
) -> Result<JsonValue, DatastoreError> {
    let mut request = elastic::request::new_request();
    filters.push(elastic::request::term_filter(
        &ds.map_field("dhcp.type"),
        "reply",
    ));
    request.set_filters(filters);

    let aggs = json!({
        "servers": {
          "terms": {
            "field": ds.map_field("src_ip"),
            "size": 10000
          },
        }
    });

    request["aggs"] = aggs;
    request.size(0);

    let response: JsonValue = ds.search(&request).await?.json().await?;
    let mut results = Vec::new();

    if let Some(buckets) = response["aggregations"]["servers"]["buckets"].as_array() {
        for bucket in buckets {
            let entry = json!({
                "ip": bucket["key"],
                "count": bucket["doc_count"],
            });
            results.push(entry);
        }
    }

    Ok(json!({
        "data": results,
    }))
}

/// For each client MAC address seen, return a list of IP addresses the MAC has
/// been assigned.
pub async fn mac(
    ds: &EventStore,
    mut filters: Vec<JsonValue>,
) -> Result<JsonValue, DatastoreError> {
    let mut request = elastic::request::new_request();
    filters.push(elastic::request::term_filter(
        &ds.map_field("dhcp.type"),
        "reply",
    ));
    request.set_filters(filters);

    let aggs = json!({
        "client_mac": {
          "terms": {
            "field": ds.map_field("dhcp.client_mac"),
            "size": 10000
          },
          "aggs": {
            "assigned_ip": {
                "terms": {
                    "field": ds.map_field("dhcp.assigned_ip"),
                }
            }
          }
        }
    });

    request["aggs"] = aggs;
    request.size(0);

    let response: JsonValue = ds.search(&request).await?.json().await?;

    let mut results = Vec::new();

    if let JsonValue::Array(buckets) = &response["aggregations"]["client_mac"]["buckets"] {
        for bucket in buckets {
            let mut addrs = Vec::new();
            if let JsonValue::Array(buckets) = &bucket["assigned_ip"]["buckets"] {
                for v in buckets {
                    if let JsonValue::String(v) = &v["key"] {
                        // Not really interested in 0.0.0.0.
                        if v != "0.0.0.0" {
                            addrs.push(v);
                        }
                    }
                }
            }

            let entry = json!({
                "mac": bucket["key"],
                "addrs": addrs,
            });
            results.push(entry);
        }
    }

    Ok(json!({
        "data": results,
    }))
}

/// For each assigned IP address, return a list of MAC addresses that have been
/// assigned that IP address.
pub async fn ip(ds: &EventStore, mut filters: Vec<JsonValue>) -> Result<JsonValue, DatastoreError> {
    let mut request = elastic::request::new_request();
    filters.push(elastic::request::term_filter(
        &ds.map_field("dhcp.type"),
        "reply",
    ));
    request.set_filters(filters);

    let aggs = json!({
        "assigned_ip": {
          "terms": {
            "field": ds.map_field("dhcp.assigned_ip"),
            "size": 10000,
          },
          "aggs": {
            "client_mac": {
                "terms": {
                    "field": ds.map_field("dhcp.client_mac"),
                }
            }
          }
        }
    });

    request["aggs"] = aggs;
    request.size(0);

    let response: JsonValue = ds.search(&request).await?.json().await?;

    let mut results = Vec::new();

    if let JsonValue::Array(buckets) = &response["aggregations"]["assigned_ip"]["buckets"] {
        for bucket in buckets {
            // Skip 0.0.0.0.
            // TODO: Filter out in the query.
            if bucket["key"] == JsonValue::String("0.0.0.0".to_string()) {
                continue;
            }

            let mut addrs = Vec::new();
            if let JsonValue::Array(buckets) = &bucket["client_mac"]["buckets"] {
                for v in buckets {
                    if let JsonValue::String(v) = &v["key"] {
                        addrs.push(v);
                    }
                }
            }

            let entry = json!({
                "ip": bucket["key"],
                "macs": addrs,
            });
            results.push(entry);
        }
    }

    Ok(json!({
        "data": results,
    }))
}

fn map_dhcp_event(event: &JsonValue, ecs: bool) -> JsonValue {
    if ecs {
        json!({
            "timestamp": event["@timestamp"],
            "sensor": event["agent"]["hostname"],
            "client_mac": event["suricata"]["eve"]["dhcp"]["client_mac"],
            "hostname": event["suricata"]["eve"]["dhcp"]["hostname"],
            "lease_time": event["suricata"]["eve"]["dhcp"]["lease_time"],
            "assigned_ip": event["suricata"]["eve"]["dhcp"]["assigned_ip"],
        })
    } else {
        json!({
            "timestamp": event["timestamp"],
            "sensor": event["host"],
            "client_mac": event["dhcp"]["client_mac"],
            "hostname": event["dhcp"]["hostname"],
            "lease_time": event["dhcp"]["lease_time"],
            "assigned_ip": event["dhcp"]["assigned_ip"],
        })
    }
}
