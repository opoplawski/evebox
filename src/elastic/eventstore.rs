// Copyright (C) 2020-2021 Jason Ish
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

use super::format_timestamp;
use super::query_string_query;
use super::Client;
use super::ElasticError;
use super::HistoryEntry;
use super::ACTION_ARCHIVED;
use super::ACTION_COMMENT;
use super::TAG_ESCALATED;
use crate::datastore::HistogramInterval;
use crate::datastore::{self, DatastoreError};
use crate::elastic::importer::Importer;
use crate::elastic::{
    request, AlertQueryOptions, ElasticResponse, ACTION_DEESCALATED, ACTION_ESCALATED,
    TAGS_ARCHIVED, TAGS_ESCALATED, TAG_ARCHIVED,
};
use crate::prelude::*;
use crate::searchquery;
use crate::server::api;
use crate::server::session::Session;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::Value as JsonValue;
use std::sync::Arc;

/// Elasticsearch eventstore - for searching events.
#[derive(Debug, Clone)]
pub struct EventStore {
    pub base_index: String,
    pub index_pattern: String,
    pub client: Client,
    pub ecs: bool,
    pub no_index_suffix: bool,
}

impl EventStore {
    pub fn get_importer(&self) -> Importer {
        super::importer::Importer::new(self.client.clone(), &self.base_index, false)
    }

    async fn post<T: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let path = format!("{}/{}", self.index_pattern, path);
        self.client.post(&path)?.json(body).send().await
    }

    pub async fn search<T: Serialize + ?Sized>(
        &self,
        body: &T,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let path = "_search?rest_total_hits_as_int=true&";
        self.post(path, body).await
    }

    /// Map field names as needed.
    ///
    /// For ECS some of the field names have been changed completely. This happens when using
    /// Filebeat with the Suricata module.
    ///
    /// For plain old Eve in Elasticsearch we still need to map some fields to their keyword
    /// variant.
    pub fn map_field(&self, name: &str) -> String {
        if self.ecs {
            match name {
                "dest_ip" => "destination.address".to_string(),
                "dest_port" => "destination.port".to_string(),
                "dns.rrname" => "dns.question.name".to_string(),
                "dns.rrtype" => "dns.question.type".to_string(),
                "dns.rcode" => "dns.response_code".to_string(),
                "dns.type" => name.to_string(),
                "src_ip" => "source.address".to_string(),
                "src_port" => "source.port".to_string(),
                _ => {
                    if name.starts_with("suricata") {
                        // Don't remap.
                        name.to_string()
                    } else {
                        format!("suricata.eve.{}", name)
                    }
                }
            }
        } else {
            match name {
                "alert.category" => "alert.category.keyword".to_string(),
                "alert.signature" => "alert.signature.keyword".to_string(),
                "app_proto" => "app_proto.keyword".to_string(),
                "dest_ip" => "dest_ip.keyword".to_string(),
                "dhcp.assigned_ip" => "dhcp.assigned_ip.keyword".to_string(),
                "dhcp.client_mac" => "dhcp.client_mac.keyword".to_string(),
                "dns.rrname" => "dns.rrname.keyword".to_string(),
                "dns.rrtype" => "dns.rrtype.keyword".to_string(),
                "dns.rcode" => "dns.rcode.keyword".to_string(),
                "dns.rdata" => "dns.rdata.keyword".to_string(),
                "host" => "host.keyword".to_string(),
                "src_ip" => "src_ip.keyword".to_string(),
                "ssh.client.software_version" => "ssh.client.software_version.keyword".to_string(),
                "ssh.server.software_version" => "ssh.server.software_version.keyword".to_string(),
                "traffic.id" => "traffic.id.keyword".to_string(),
                "traffic.label" => "traffic.label.keyword".to_string(),
                _ => name.to_string(),
            }
        }
    }

    async fn add_tag_by_query(
        &self,
        query: serde_json::Value,
        tag: &str,
        action: &HistoryEntry,
    ) -> Result<(), DatastoreError> {
        self.add_tags_by_query(query, &[tag], action).await
    }

    async fn add_tags_by_query(
        &self,
        query: serde_json::Value,
        tags: &[&str],
        action: &HistoryEntry,
    ) -> Result<(), DatastoreError> {
        let script = json!({
            "lang": "painless",
            "inline": "
                if (params.tags != null) {
                    if (ctx._source.tags == null) {
                        ctx._source.tags = new ArrayList();
                    }
                    for (tag in params.tags) {
                        if (!ctx._source.tags.contains(tag)) {
                            ctx._source.tags.add(tag);
                        }
                    }
                }
                if (ctx._source.evebox == null) {
                    ctx._source.evebox = new HashMap();
                }
                if (ctx._source.evebox.history == null) {
                    ctx._source.evebox.history = new ArrayList();
                }
                ctx._source.evebox.history.add(params.action);
            ",
            "params": {
                "tags":   tags,
                "action": action,
            },
        });
        let body = json!({
            "query": query,
            "script": script,
        });

        let path = "_update_by_query?refresh=true&conflicts=proceed";
        let response: ElasticResponse = self.post(path, &body).await?.json().await?;
        let updated = if let Some(updated) = response.updated {
            updated
        } else {
            0
        };
        if updated == 0 {
            warn!(?response, "No events updated");
        }

        Ok(())
    }

    async fn remove_tag_by_query(
        &self,
        query: serde_json::Value,
        tag: &str,
        action: &HistoryEntry,
    ) -> Result<(), DatastoreError> {
        self.remove_tags_by_query(query, &[tag], action).await
    }

    async fn remove_tags_by_query(
        &self,
        query: serde_json::Value,
        tags: &[&str],
        action: &HistoryEntry,
    ) -> Result<(), DatastoreError> {
        let script = json!({
            "lang": "painless",
            "inline": "
                if (ctx._source.tags != null) {
                    for (tag in params.tags) {
                        ctx._source.tags.removeIf(entry -> entry == tag);
                    }
                }
                if (ctx._source.evebox == null) {
                    ctx._source.evebox = new HashMap();
                }
                if (ctx._source.evebox.history == null) {
                    ctx._source.evebox.history = new ArrayList();
                }
                ctx._source.evebox.history.add(params.action);
            ",
            "params": {
                "tags":   tags,
                "action": action,
            },
        });
        let body = json!({
            "query": query,
            "script": script,
        });
        let path = "_update_by_query?refresh=true&conflicts=proceed";
        let _reponse = self.post(path, &body).await?.bytes().await?;
        Ok(())
    }

    async fn add_tags_by_alert_group(
        &self,
        alert_group: api::AlertGroupSpec,
        tags: &[&str],
        action: &HistoryEntry,
    ) -> Result<(), DatastoreError> {
        let mut must_not = Vec::new();
        for tag in tags {
            must_not.push(json!({"term": {"tags": tag}}));
        }

        let query = json!({
            "bool": {
                "filter": self.build_alert_group_filter(&alert_group),
                "must_not": must_not,
            }
        });

        self.add_tags_by_query(query, tags, action).await
    }

    async fn remove_tags_by_alert_group(
        &self,
        alert_group: api::AlertGroupSpec,
        tags: &[&str],
        action: &HistoryEntry,
    ) -> Result<(), DatastoreError> {
        let mut filters = self.build_alert_group_filter(&alert_group);
        for tag in tags {
            filters.push(json!({"term": {"tags": tag}}));
        }
        let query = json!({
            "bool": {
                "filter": filters,
            }
        });
        self.remove_tags_by_query(query, tags, action).await
    }

    pub async fn archive_event_by_id(&self, event_id: &str) -> Result<(), DatastoreError> {
        let query = json!({
            "bool": {
                "filter": {
                    "term": {"_id": event_id}
                }
            }
        });
        let action = HistoryEntry {
            username: "anonymous".to_string(),
            timestamp: format_timestamp(chrono::Utc::now()),
            action: ACTION_ARCHIVED.to_string(),
            comment: None,
        };
        self.add_tag_by_query(query, TAG_ARCHIVED, &action).await
    }

    pub async fn escalate_event_by_id(&self, event_id: &str) -> Result<(), DatastoreError> {
        let query = json!({
            "bool": {
                "filter": {
                    "term": {"_id": event_id}
                }
            }
        });
        let action = HistoryEntry {
            username: "anonymous".to_string(),
            timestamp: format_timestamp(chrono::Utc::now()),
            action: ACTION_ESCALATED.to_string(),
            comment: None,
        };
        self.add_tag_by_query(query, TAG_ESCALATED, &action).await
    }

    pub async fn deescalate_event_by_id(&self, event_id: &str) -> Result<(), DatastoreError> {
        let query = json!({
            "bool": {
                "filter": {
                    "term": {"_id": event_id}
                }
            }
        });
        let action = HistoryEntry {
            username: "anonymous".to_string(),
            timestamp: format_timestamp(chrono::Utc::now()),
            action: ACTION_DEESCALATED.to_string(),
            comment: None,
        };
        self.remove_tag_by_query(query, TAG_ESCALATED, &action)
            .await
    }

    pub async fn comment_event_by_id(
        &self,
        event_id: &str,
        comment: String,
        username: &str,
    ) -> Result<(), DatastoreError> {
        let query = json!({
            "bool": {
                "filter": {
                    "term": {"_id": event_id}
                }
            }
        });
        let action = HistoryEntry {
            username: username.to_string(),
            timestamp: format_timestamp(chrono::Utc::now()),
            action: ACTION_COMMENT.to_string(),
            comment: Some(comment),
        };
        self.add_tags_by_query(query, &[], &action).await
    }

    pub async fn get_event_by_id(
        &self,
        event_id: String,
    ) -> Result<Option<serde_json::Value>, DatastoreError> {
        let query = json!({
            "query": {
                "bool": {
                    "filter": {
                        "term": {"_id": event_id}
                    }
                }
            }
        });
        let response: ElasticResponse = self.search(&query).await?.json().await?;
        if let Some(error) = response.error {
            return Err(ElasticError::ErrorResponse(error.reason).into());
        } else if let Some(hits) = &response.hits {
            if let serde_json::Value::Array(hits) = &hits["hits"] {
                if !hits.is_empty() {
                    return Ok(Some(hits[0].clone()));
                } else {
                    return Ok(None);
                }
            }
        }

        // If we get here something in the response was unexpected.
        warn!(
            "Received unexpected response for get_event_by_id from Elastic Search: {:?}",
            response
        );
        Ok(None)
    }

    fn query_string_to_filters(&self, query: &str) -> Vec<serde_json::Value> {
        let mut filters = Vec::new();
        match searchquery::parse(query) {
            Err(err) => {
                error!("Failed to parse query string: {} -- {}", &query, err);
            }
            Ok((_, elements)) => {
                for element in elements {
                    match element {
                        searchquery::Element::KeyVal(key, val) => {
                            filters.push(request::term_filter(&self.map_field(&key), &val));
                        }
                        searchquery::Element::String(val) => {
                            filters.push(query_string_query(&val));
                        }
                    }
                }
            }
        }
        filters
    }

    pub fn build_inbox_query(&self, options: AlertQueryOptions) -> serde_json::Value {
        let mut filters = Vec::new();
        filters.push(json!({"exists": {"field": self.map_field("event_type")}}));
        filters.push(json!({"term": {self.map_field("event_type"): "alert"}}));
        if let Some(timestamp_gte) = options.timestamp_gte {
            filters
                .push(json!({"range": {"@timestamp": {"gte": format_timestamp(timestamp_gte)}}}));
        }
        if let Some(query_string) = options.query_string {
            filters.extend(self.query_string_to_filters(&query_string));
        }

        let mut must_not = Vec::new();
        for tag in options.tags {
            if let Some(tag) = tag.strip_prefix('-') {
                if tag == "archived" {
                    debug!("Rewriting tag {} to {}", tag, "evebox.archived");
                    must_not.push(json!({"term": {"tags": "evebox.archived"}}));
                } else {
                    let j = json!({"term": {"tags": tag}});
                    must_not.push(j);
                }
            } else if tag == "escalated" {
                debug!("Rewriting tag {} to {}", tag, "evebox.escalated");
                let j = json!({"term": {"tags": "evebox.escalated"}});
                filters.push(j);
            } else {
                let j = json!({"term": {"tags": tag}});
                filters.push(j);
            }
        }

        let query = json!({
            "query": {
                "bool": {
                    "filter": filters,
                    "must_not": must_not,
                }
            },
            "sort": [{"@timestamp": {"order": "desc"}}],
            "aggs": {
                "signatures": {
                    "terms": {"field": self.map_field("alert.signature_id"), "size": 2000},
                    "aggs": {
                        "sources": {
                            "terms": {"field": self.map_field("src_ip"), "size": 1000},
                            "aggs": {
                                "destinations": {
                                    "terms": {"field": self.map_field("dest_ip"), "size": 500},
                                    "aggs": {
                                        "escalated": {"filter": {"term": {"tags": "evebox.escalated"}}},
                                        "newest": {"top_hits": {"size": 1, "sort": [{"@timestamp": {"order": "desc"}}]}},
                                        "oldest": {"top_hits": {"size": 1, "sort": [{"@timestamp": {"order": "asc"}}]}}
                                    },
                                },
                            },
                        },
                    },
                }
            }
        });

        query
    }

    pub async fn alert_query(
        &self,
        options: AlertQueryOptions,
    ) -> Result<serde_json::Value, DatastoreError> {
        let query = self.build_inbox_query(options);
        let body = self.search(&query).await?.text().await?;
        let response: ElasticResponse = serde_json::from_str(&body)?;
        if let Some(error) = response.error {
            return Err(DatastoreError::ElasticSearchError(error.reason));
        }

        let mut alerts = Vec::new();
        if let Some(aggregrations) = response.aggregations {
            if let JsonValue::Array(buckets) = &aggregrations["signatures"]["buckets"] {
                for bucket in buckets {
                    if let JsonValue::Array(buckets) = &bucket["sources"]["buckets"] {
                        for bucket in buckets {
                            if let JsonValue::Array(buckets) = &bucket["destinations"]["buckets"] {
                                for bucket in buckets {
                                    let newest = &bucket["newest"]["hits"]["hits"][0];
                                    let oldest = &bucket["oldest"]["hits"]["hits"][0];
                                    let escalated = &bucket["escalated"]["doc_count"];
                                    let record = json!({
                                        "count": bucket["doc_count"],
                                        "event": newest,
                                        "escalatedCount": escalated,
                                        "maxTs": &newest["_source"]["@timestamp"],
                                        "minTs": &oldest["_source"]["@timestamp"],
                                    });
                                    alerts.push(record);
                                }
                            }
                        }
                    }
                }
            }
        } else {
            warn!("Elasticsearch response has no aggregations");
        }

        let response = json!({
            "ecs": self.ecs,
            "alerts": alerts,
        });

        Ok(response)
    }

    pub async fn archive_by_alert_group(
        &self,
        alert_group: api::AlertGroupSpec,
    ) -> Result<(), DatastoreError> {
        let action = HistoryEntry {
            username: "anonymous".to_string(),
            timestamp: format_timestamp(chrono::offset::Utc::now()),
            action: ACTION_ARCHIVED.to_string(),
            comment: None,
        };
        self.add_tags_by_alert_group(alert_group, &TAGS_ARCHIVED, &action)
            .await
    }

    pub async fn escalate_by_alert_group(
        &self,
        alert_group: api::AlertGroupSpec,
        session: Arc<Session>,
    ) -> Result<(), DatastoreError> {
        let action = HistoryEntry {
            username: session.username().to_string(),
            //username: "anonymous".to_string(),
            timestamp: format_timestamp(chrono::offset::Utc::now()),
            action: ACTION_ESCALATED.to_string(),
            comment: None,
        };
        self.add_tags_by_alert_group(alert_group, &TAGS_ESCALATED, &action)
            .await
    }

    pub async fn deescalate_by_alert_group(
        &self,
        alert_group: api::AlertGroupSpec,
    ) -> Result<(), DatastoreError> {
        let action = HistoryEntry {
            username: "anonymous".to_string(),
            timestamp: format_timestamp(chrono::offset::Utc::now()),
            action: ACTION_DEESCALATED.to_string(),
            comment: None,
        };
        self.remove_tags_by_alert_group(alert_group, &TAGS_ESCALATED, &action)
            .await
    }

    pub async fn event_query(
        &self,
        params: datastore::EventQueryParams,
    ) -> Result<serde_json::Value, DatastoreError> {
        let mut filters = vec![request::exists_filter(&self.map_field("event_type"))];

        if let Some(event_type) = params.event_type {
            filters.push(request::term_filter(
                &self.map_field("event_type"),
                &event_type,
            ));
        }

        if let Some(query_string) = params.query_string {
            filters.extend(self.query_string_to_filters(&query_string));
        }

        if let Some(timestamp) = params.min_timestamp {
            filters.push(request::timestamp_gte_filter(timestamp));
        }

        if let Some(timestamp) = params.max_timestamp {
            filters.push(request::timestamp_lte_filter(timestamp));
        }

        let sort_by = params.sort_by.unwrap_or_else(|| "@timestamp".to_string());
        let sort_order = params.order.unwrap_or_else(|| "desc".to_string());
        let size = params.size.unwrap_or(500);

        let body = json!({
            "query": {
                "bool": {
                    "filter": filters,
                }
            },
            "sort": [{sort_by: {"order": sort_order}}],
            "size": size,
        });
        let response: JsonValue = self.search(&body).await?.json().await?;
        let hits = &response["hits"]["hits"];

        let response = json!({
            "ecs": self.ecs,
            "data": hits,
        });

        Ok(response)
    }

    pub async fn comment_by_alert_group(
        &self,
        alert_group: api::AlertGroupSpec,
        comment: String,
        username: &str,
    ) -> Result<(), DatastoreError> {
        let entry = HistoryEntry {
            username: username.to_string(),
            timestamp: format_timestamp(chrono::Utc::now()),
            action: ACTION_COMMENT.to_string(),
            comment: Some(comment),
        };
        self.add_tags_by_alert_group(alert_group, &[], &entry).await
    }

    pub async fn histogram(
        &self,
        params: datastore::HistogramParameters,
    ) -> Result<serde_json::Value, DatastoreError> {
        let mut bound_max = None;
        let mut bound_min = None;
        let mut filters = vec![request::exists_filter(&self.map_field("event_type"))];
        if let Some(ts) = params.min_timestamp {
            filters.push(request::timestamp_gte_filter(ts));
            bound_min = Some(format_timestamp(ts));
        }
        if let Some(ts) = params.max_timestamp {
            filters.push(json!({"range":{"@timestamp":{"lte":format_timestamp(ts)}}}));
            bound_max = Some(format_timestamp(ts));
        }
        if let Some(event_type) = params.event_type {
            filters.push(request::term_filter(
                &self.map_field("event_type"),
                &event_type,
            ));
        }
        if let Some(dns_type) = params.dns_type {
            filters.push(request::term_filter(&self.map_field("dns.type"), &dns_type));
        }

        if let Some(query_string) = params.query_string {
            filters.extend(self.query_string_to_filters(&query_string));
        }

        if !self.ecs {
            if let Some(sensor_name) = params.sensor_name {
                if !sensor_name.is_empty() {
                    filters.push(request::term_filter(&self.map_field("host"), &sensor_name));
                }
            }
        }

        let mut should = Vec::new();
        let mut min_should_match = 0;
        if let Some(address_filter) = params.address_filter {
            should.push(request::term_filter(
                &self.map_field("src_ip"),
                &address_filter,
            ));
            should.push(request::term_filter(
                &self.map_field("dest_ip"),
                &address_filter,
            ));
            min_should_match = 1;
        }

        let interval = match params.interval {
            Some(interval) => match interval {
                HistogramInterval::Minute => "1m",
                HistogramInterval::Hour => "1h",
                HistogramInterval::Day => "1d",
            },
            None => "1h",
        };

        let major_version = match self.client.get_version().await {
            Ok(version) => version.major,
            Err(_) => 6,
        };
        let events_over_time = if major_version < 7 {
            json!({
                "date_histogram": {
                    "field": "@timestamp",
                    "interval": interval,
                    "min_doc_count": 0,
                    "extended_bounds": {
                        "max": bound_max,
                        "min": bound_min,
                    },
                }
            })
        } else {
            json!({
                "date_histogram": {
                    "field": "@timestamp",
                    "calendar_interval": interval,
                    "min_doc_count": 0,
                    "extended_bounds": {
                        "max": bound_max,
                        "min": bound_min,
                    },
                }
            })
        };

        let body = json!({
            "query": {
                "bool": {
                    "filter": filters,
                    "must_not": [{"term": {self.map_field("event_type"): "stats"}}],
                    "should": should,
                    "minimum_should_match": min_should_match,
                },
            },
            "size": 0,
            "sort":[{"@timestamp":{"order":"desc"}}],
            "aggs": {
                "events_over_time": events_over_time,
            }
        });

        let response: JsonValue = self.search(&body).await?.json().await?;
        let buckets = &response["aggregations"]["events_over_time"]["buckets"];
        let mut data = Vec::new();
        if let JsonValue::Array(buckets) = buckets {
            for bucket in buckets {
                data.push(json!({
                    "key": bucket["key"],
                    "count": bucket["doc_count"],
                    "key_as_string": bucket["key_as_string"],
                }));
            }
        }

        let response = json!({
            "data": data,
        });

        Ok(response)
    }

    pub async fn agg(&self, params: datastore::AggParameters) -> Result<JsonValue, DatastoreError> {
        let mut filters = vec![request::exists_filter(&self.map_field("event_type"))];
        if let Some(event_type) = params.event_type {
            filters.push(request::term_filter(
                &self.map_field("event_type"),
                &event_type,
            ));
        }
        if let Some(dns_type) = params.dns_type {
            filters.push(request::term_filter(&self.map_field("dns.type"), &dns_type));
        }
        if let Some(ts) = params.min_timestamp {
            filters.push(request::timestamp_gte_filter(ts));
        }

        let mut should = Vec::new();
        let mut min_should_match = 0;

        if let Some(address_filter) = params.address_filter {
            should.push(request::term_filter(
                &self.map_field("src_ip"),
                &address_filter,
            ));
            should.push(request::term_filter(
                &self.map_field("dest_ip"),
                &address_filter,
            ));
            min_should_match = 1;
        }

        if let Some(query_string) = params.query_string {
            filters.extend(self.query_string_to_filters(&query_string));
        }

        let agg = self.map_field(&params.agg);

        let query = json!({
            "query": {
                "bool": {
                    "filter": filters,
                    "should": should,
                    "minimum_should_match": min_should_match,
                },
            },
            "size": 0,
            "sort": [{"@timestamp":{"order":"desc"}}],
            "aggs": {
                "agg": {
                    "terms": {
                        "field": agg,
                        "size": params.size,
                    }
                },
                "missing": {
                    "missing": {
                        "field": agg,
                    }
                }
            }
        });

        let response: JsonValue = self.search(&query).await?.json().await?;

        let mut data = vec![];
        if let JsonValue::Array(buckets) = &response["aggregations"]["agg"]["buckets"] {
            for bucket in buckets {
                let entry = json!({
                    "key": bucket["key"],
                    "count": bucket["doc_count"],
                });
                data.push(entry);
            }
        }

        let response = json!({
            "data": data,
        });

        Ok(response)
    }

    pub async fn flow_histogram(
        &self,
        params: datastore::FlowHistogramParameters,
    ) -> Result<JsonValue, datastore::DatastoreError> {
        let mut filters = vec![
            request::term_filter(&self.map_field("event_type"), "flow"),
            request::exists_filter(&self.map_field("event_type")),
        ];
        if let Some(mints) = params.mints {
            filters.push(request::timestamp_gte_filter(mints));
        }
        if let Some(query_string) = params.query_string {
            filters.extend(self.query_string_to_filters(&query_string));
        }
        let query = json!({
            "query": {
                "bool": {
                    "filter": filters,
                }
            },
            "sort":[{"@timestamp":{"order":"desc"}}],
            "aggs": {
                "histogram": {
                    "aggs": {
                        "app_proto": {
                            "terms":{
                                "field": self.map_field("app_proto"),
                            }
                        }
                    },
                    "date_histogram": {
                        "field": "@timestamp",
                        "interval": params.interval,
                    }
                }
            }
        });
        let response: JsonValue = self.search(&query).await?.json().await?;
        let mut data = Vec::new();
        if let JsonValue::Array(buckets) = &response["aggregations"]["histogram"]["buckets"] {
            for bucket in buckets {
                let mut entry = json!({
                    "key": bucket["key"],
                    "events": bucket["doc_count"],
                });
                if let JsonValue::Array(buckets) = &bucket["app_proto"]["buckets"] {
                    let mut app_protos = json!({});
                    for bucket in buckets {
                        if let Some(key) = bucket["key"].as_str() {
                            if let Some(count) = bucket["doc_count"].as_u64() {
                                app_protos[key] = count.into();
                            }
                        }
                    }
                    entry["app_proto"] = app_protos;
                }
                data.push(entry);
            }
        }

        let response = json!({
            "data": data,
        });

        return Ok(response);
    }

    fn build_alert_group_filter(&self, request: &api::AlertGroupSpec) -> Vec<JsonValue> {
        let mut filter = Vec::new();
        filter.push(json!({"exists": {"field": self.map_field("event_type")}}));
        filter.push(json!({"term": {self.map_field("event_type"): "alert"}}));
        filter.push(json!({
            "range": {
                "@timestamp": {
                    "gte": request.min_timestamp,
                    "lte": request.max_timestamp,
                }
            }
        }));
        filter.push(json!({"term": {self.map_field("src_ip"): request.src_ip}}));
        filter.push(json!({"term": {self.map_field("dest_ip"): request.dest_ip}}));
        filter.push(json!({"term": {self.map_field("alert.signature_id"): request.signature_id}}));
        return filter;
    }

    pub async fn get_sensors(&self) -> anyhow::Result<Vec<String>> {
        let request = json!({
            "size": 0,
            "aggs": {
                "sensors": {
                    "terms": {
                        "field": self.map_field("host"),
                    }
                }
            }
        });
        let mut response: serde_json::Value = self.search(&request).await?.json().await?;
        let buckets = response["aggregations"]["sensors"]["buckets"].take();

        #[derive(Deserialize, Debug)]
        struct Bucket {
            key: String,
        }

        let buckets: Vec<Bucket> = serde_json::from_value(buckets)?;
        let sensors: Vec<String> = buckets.iter().map(|b| b.key.to_string()).collect();
        return Ok(sensors);
    }

    pub async fn stats_agg(
        &self,
        params: datastore::StatsAggQueryParams,
    ) -> anyhow::Result<serde_json::Value> {
        let version = self.client.get_version().await?;
        let date_histogram_interval_field_name = if version.major < 7 {
            "interval"
        } else {
            "fixed_interval"
        };
        let start_time = params
            .start_time
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let interval = elastic_format_interval(params.interval);
        let mut filters = vec![];
        filters.push(json!({"term": {self.map_field("event_type"): "stats"}}));
        filters.push(json!({"range": {"@timestamp": {"gte": start_time}}}));
        if let Some(sensor_name) = params.sensor_name {
            filters.push(json!({"term": {"host": sensor_name}}));
        }
        let field = self.map_field(&params.field);
        let query = json!({
           "query": {
                "bool": {
                    "filter": filters,
                }
            },
            "size": 0,
            "sort": [{"@timestamp": {"order": "asc"}}],
            "aggs": {
               "histogram": {
                  "date_histogram": {
                    "field": "@timestamp",
                    date_histogram_interval_field_name: interval,
                    },
              "aggs": {
                "memuse": {
                  "max": {
                    "field": field,
                  }
                },
            }}}
        });
        let mut response: serde_json::Value = self.search(&query).await?.json().await?;

        #[derive(Debug, Deserialize, Serialize, Default)]
        struct Value {
            value: Option<f64>,
        }

        #[derive(Debug, Deserialize, Serialize, Default)]
        struct Bucket {
            key_as_string: String,
            memuse: Value,
        }

        let buckets = response["aggregations"]["histogram"]["buckets"].take();
        let buckets: Vec<Bucket> = serde_json::from_value(buckets)?;
        let buckets: Vec<serde_json::Value> = buckets
            .iter()
            .map(|b| {
                json!({
                    "timestamp": b.key_as_string,
                    "value": b.memuse.value.unwrap_or(0.0) as u64,
                })
            })
            .collect();
        let response = json!({
            "data": buckets,
        });

        return Ok(response);
    }

    pub async fn stats_agg_deriv(
        &self,
        params: &datastore::StatsAggQueryParams,
    ) -> anyhow::Result<serde_json::Value> {
        let version = self.client.get_version().await.unwrap();
        let date_histogram_interval_field_name = if version.major < 7 {
            "interval"
        } else {
            "fixed_interval"
        };
        let start_time = params
            .start_time
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let interval = elastic_format_interval(params.interval);
        let mut filters = vec![];
        filters.push(json!({"term": {self.map_field("event_type"): "stats"}}));
        filters.push(json!({"range": {"@timestamp": {"gte": start_time}}}));
        if let Some(sensor_name) = &params.sensor_name {
            filters.push(json!({"term": {self.map_field("host"): sensor_name}}));
        }
        let field = self.map_field(&params.field);
        let query = json!({
          "query": {
            "bool": {
              "filter": filters,
            }
          },
          "size": 0,
          "sort": [{"@timestamp": {"order": "asc"}}],
          "aggs": {
            "histogram": {
              "date_histogram": {
                "field": "@timestamp",
                date_histogram_interval_field_name: interval,
              },
              "aggs": {
                "values": {
                  "max": {
                    "field": field,
                  }
                },
                "values_deriv": {
                  "derivative": {
                    "buckets_path": "values"
                  }
                }
              }
            }
          }
        });

        let response = self.search(&query).await?;
        if response.status() != 200 {
            let error_text = response.text().await?;
            anyhow::bail!(error_text);
        }
        let mut response: serde_json::Value = response.json().await?;

        #[derive(Debug, Deserialize, Serialize, Default)]
        struct Value {
            value: Option<f64>,
        }

        #[derive(Debug, Deserialize, Serialize)]
        struct Bucket {
            key_as_string: String,
            key: u64,
            values_deriv: Option<Value>,
        }

        let buckets = response["aggregations"]["histogram"]["buckets"].take();
        let buckets: Vec<Bucket> = serde_json::from_value(buckets)?;
        let response_data: Vec<serde_json::Value> = buckets
            .iter()
            .map(|b| {
                let bytes = b
                    .values_deriv
                    .as_ref()
                    .and_then(|v| v.value.as_ref())
                    .copied()
                    .unwrap_or(0.0) as u64;
                json!({
                    "timestamp": b.key_as_string,
                    "value": bytes,
                })
            })
            .collect();
        let response = json!({
            "data": response_data,
        });
        Ok(response)
    }
}

fn elastic_format_interval(duration: time::Duration) -> String {
    let result = if duration < time::Duration::minutes(1) {
        format!("{}s", duration.whole_seconds())
    } else {
        format!("{}m", duration.whole_minutes())
    };
    debug!("Formatted duration of {:?} as {}", duration, &result);
    result
}
