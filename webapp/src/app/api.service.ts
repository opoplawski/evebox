// Copyright (C) 2014-2020 Jason Ish
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
// IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
// OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE
// OR OTHER DEALINGS IN THE SOFTWARE.

import { Injectable } from "@angular/core";
import { Router } from "@angular/router";
import { ConfigService } from "./config.service";
import { Observable } from "rxjs";
import { ClientService, LoginResponse } from "./client.service";
import { catchError, finalize, map } from "rxjs/operators";
import { HttpClient, HttpHeaders, HttpParams } from "@angular/common/http";
import { throwError } from "rxjs/internal/observable/throwError";

declare var localStorage: any;

const SESSION_HEADER = "x-evebox-session-id";

/**
 * The API service exposes the server side API to the rest of the server,
 * and acts as the "client" to the server.
 */
@Injectable()
export class ApiService {

    private authenticated = false;

    constructor(private httpClient: HttpClient,
                public client: ClientService,
                private router: Router,
                private configService: ConfigService) {
        this.client._sessionId = localStorage._sessionId;
    }

    isAuthenticated(): boolean {
        return this.authenticated;
    }

    setSessionId(sessionId: string | null): void {
        this.client.setSessionId(sessionId);
    }

    checkVersion(response: any): void {
        this.client.checkVersion(response);
    }

    applySessionHeader(options: any): void {
        if (this.client._sessionId) {
            const headers = options.headers || new Headers();
            headers.append(SESSION_HEADER, this.client._sessionId);
            options.headers = headers;
        }
    }

    setSessionHeader(headers: HttpHeaders): HttpHeaders {
        if (this.client._sessionId) {
            return headers.set(SESSION_HEADER, this.client._sessionId);
        }
        return headers;
    }

    setAuthenticated(authenticated: boolean): void {
        console.log(`Setting authenticated to ${authenticated}`);
        this.authenticated = authenticated;
        this.client.setAuthenticated(authenticated);
        if (!authenticated) {
            this.setSessionId(null);
            this.router.navigate(["/login"]).then(() => {
            });
        }
    }

    private handle401(): void {
        this.setAuthenticated(false);
    }

    /**
     * Low level options request, just fixup the URL.
     */
    _options(path: string): Observable<any> {
        return this.httpClient.options(this.client.buildUrl(path));
    }

    doRequest(method: string, path: string, options: any = {}): Observable<any> {
        const headers = options.headers || new HttpHeaders();
        options.headers = this.setSessionHeader(headers);
        options.observe = "response";
        return this.httpClient.request<any>(method, path, options)
            .pipe(map((response: any) => {
                this.client.updateSessionId(response);
                this.checkVersion(response);
                return response.body;
            }), catchError((error) => {
                if (error.error instanceof ErrorEvent) {
                    // Client side or network error.
                } else {
                    if (error.status === 401) {
                        this.handle401();
                    }
                }
                return throwError(error);
            }));
    }

    post(path: string, body: any, options: any = {}): Promise<any> {
        options.body = body;
        return this.doRequest("POST", path, options).toPromise();
    }

    updateConfig(): Promise<any> {
        return this.client.get("api/1/config").toPromise()
            .then((config) => {
                console.log("got config");
                console.log(config);
                this.configService.setConfig(config);
                return config;
            });
    }

    checkAuth(): Promise<true | false> {
        console.log(this.client._sessionId);
        return this.updateConfig()
            .then(() => {
                this.setAuthenticated(true);
                return true;
            })
            .catch((error) => {
                console.log(`Authentication failed: ${error.message}`);
                this.router.navigate(["/login"]);
                return false;
            });
    }

    login(username: string = "", password: string = ""): Promise<boolean> {
        return this.client.login(username, password).toPromise()
            .then((response: LoginResponse) => {
                this.setSessionId(response.session_id);
                console.log("Login successful, updating configuration");
                return this.updateConfig()
                    .then(() => {
                        this.setAuthenticated(true);
                        return true;
                    });
            });
    }

    logout(): Promise<any> {
        return this.client.logout().pipe(
            finalize(() => {
                this.setAuthenticated(false);
            })
        ).toPromise();
    }

    getWithParams(path: string, params = {}): Promise<any> {

        const qsb: any = [];

        for (const param of Object.keys(params)) {
            qsb.push(`${param}=${params[param]}`);
        }

        return this.client.get(`${path}?${qsb.join("&")}`).toPromise();
    }

    getVersion(): Promise<any> {
        return this.client.get("api/1/version").toPromise();
    }

    eventToPcap(what: any, event: any): void {
        // Set a cook with the session key to expire in 60 seconds from now.
        const expires = new Date(new Date().getTime() + 60000);
        const cookie = `${SESSION_HEADER}=${this.client._sessionId}; expires=${expires.toUTCString()}`;
        console.log("Setting cookie: " + cookie);
        document.cookie = cookie;

        const form = document.createElement("form") as HTMLFormElement;
        form.setAttribute("method", "post");
        form.setAttribute("action", "api/1/eve2pcap");

        const whatField = document.createElement("input") as HTMLElement;
        whatField.setAttribute("type", "hidden");
        whatField.setAttribute("name", "what");
        whatField.setAttribute("value", what);
        form.appendChild(whatField);

        const eventField = document.createElement("input") as HTMLElement;
        eventField.setAttribute("type", "hidden");
        eventField.setAttribute("name", "event");
        eventField.setAttribute("value", JSON.stringify(event));
        form.appendChild(eventField);

        document.body.appendChild(form);
        form.submit();
    }

    reportHistogram(options: ReportHistogramOptions = {}): Promise<any> {
        const query: any = [];

        if (options.timeRange && options.timeRange > 0) {
            query.push(`time_range=${options.timeRange}s`);
        }

        if (options.interval) {
            query.push(`interval=${options.interval}`);
        }

        if (options.addressFilter) {
            query.push(`address_filter=${options.addressFilter}`);
        }

        if (options.queryString) {
            query.push(`query_string=${options.queryString}`);
        }

        if (options.sensorFilter) {
            query.push(`sensor_name=${options.sensorFilter}`);
        }

        if (options.dnsType) {
            query.push(`dns_type=${options.dnsType}`);
        }

        if (options.eventType) {
            query.push(`event_type=${options.eventType}`);
        }

        return this.client.get(`api/1/report/histogram?${query.join("&")}`).toPromise();
    }

    reportAgg(agg: string, options: ReportAggOptions = {}): Promise<any> {
        let params = new HttpParams().append("agg", agg);

        for (const key of Object.keys(options)) {
            switch (key) {
                case "timeRange":
                    params = params.append("time_range", `${options[key]}s`);
                    break;
                case "queryString":
                    params = params.append("query_string", `${options[key]}`);
                    break;
                    case "eventType":
                    params = params.append("event_type", `${options[key]}`);
                    break;
                default:
                    params = params.append(key, options[key]);
                    break;
            }
        }

        return this.client.get("api/1/report/agg", params).toPromise();
    }

    /**
     * Find events - all events, not just alerts.
     */
    eventQuery(options: EventQueryOptions = {}): Observable<any> {

        let params = new HttpParams();

        if (options.queryString) {
            params = params.append("query_string", options.queryString);
        }

        if (options.maxTs) {
            params = params.append("max_ts", options.maxTs);
        }

        if (options.minTs) {
            params = params.append("min_ts", options.minTs);
        }

        if (options.eventType && options.eventType !== "all") {
            params = params.append("event_type", options.eventType);
        }

        if (options.sortOrder) {
            params = params.append("order", options.sortOrder);
        }

        if (options.sortBy) {
            params = params.append("sort_by", options.sortBy);
        }

        if (options.size) {
            params = params.append("size", options.size.toString());
        }

        if (options.timeRange) {
            params = params.append("time_range", `${options.timeRange}s`);
        }

        return this.client.get("api/1/event-query", params);
    }

    flowHistogram(args: any = {}): any {
        let params = new HttpParams();

        const subAggs = [];
        if (args.appProto) {
            subAggs.push("app_proto");
        }
        if (subAggs.length > 0) {
            params = params.append("sub_aggs", subAggs.join(","));
        }

        if (args.timeRange) {
            params = params.append("time_range", args.timeRange);
        }

        if (args.queryString) {
            params = params.append("query_string", args.queryString);
        }

        if (args.interval) {
            params = params.append("interval", args.interval);
        }

        return this.client.get("api/1/flow/histogram", params);
    }

    commentOnEvent(eventId: string, comment: string): Promise<any> {
        console.log(`Commenting on event ${eventId}.`);
        return this.post(`api/1/event/${eventId}/comment`, {
            event_id: eventId,
            comment,
        });
    }

    commentOnAlertGroup(alertGroup: any, comment: string): Promise<any> {
        console.log(`Commenting on alert group:`);
        console.log(alertGroup);

        const request = {
            signature_id: alertGroup.event._source.alert.signature_id,
            src_ip: alertGroup.event._source.src_ip,
            dest_ip: alertGroup.event._source.dest_ip,
            min_timestamp: alertGroup.minTs,
            max_timestamp: alertGroup.maxTs,
        };

        return this.post(`api/1/alert-group/comment`, {
            alert_group: request,
            comment: comment,
        });
    }

    alertQuery(options: {
        queryString?: string;
        mustHaveTags?: any[];
        mustNotHaveTags?: any[];
        timeRange?: string;
    }): Observable<any> {
        let params = new HttpParams();
        const tags: string[] = [];

        if (options.mustHaveTags) {
            options.mustHaveTags.forEach((tag: string) => {
                tags.push(tag);
            });
        }

        if (options.mustNotHaveTags) {
            options.mustNotHaveTags.forEach((tag: string) => {
                tags.push(`-${tag}`);
            });
        }

        params = params.append("tags", tags.join(","));
        params = params.append("time_range", options.timeRange);
        params = params.append("query_string", options.queryString);

        return this.client.get("api/1/alerts", params);
    }


}

export interface ReportHistogramOptions {
    timeRange?: number;
    interval?: string;
    addressFilter?: string;
    queryString?: string;
    sensorFilter?: string;
    eventType?: string;
    dnsType?: string;
}

// Options for an aggregation report.
export interface ReportAggOptions {
    size?: number;
    queryString?: string;
    timeRange?: number;

    // Event type.
    eventType?: string;

    // Subtype info.
    dnsType?: string;

}

export interface EventQueryOptions {
    queryString?: string;
    maxTs?: string;
    minTs?: string;
    eventType?: string;
    sortOrder?: string;
    sortBy?: string;
    size?: number;
    timeRange?: number;
}
