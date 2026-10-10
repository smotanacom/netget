// openzipkin/zipkin-go, unchanged, against a Zipkin collector: its tracer builds a two-span
// trace, its HTTP reporter POSTs it as v2 JSON, a second reporter sends a span the collector
// refuses, and the read API's answers are decoded with zipkin-go's own model types (whose
// UnmarshalJSON validates ids). Prints one JSON line.
//
// Usage: zipkin-peer http://host:port
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"strings"
	"time"

	zipkin "github.com/openzipkin/zipkin-go"
	"github.com/openzipkin/zipkin-go/model"
	zipkinhttp "github.com/openzipkin/zipkin-go/reporter/http"
)

func get(base, path string, into any) (int, error) {
	resp, err := http.Get(base + path)
	if err != nil {
		return 0, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return resp.StatusCode, nil
	}
	return resp.StatusCode, json.NewDecoder(resp.Body).Decode(into)
}

func main() {
	base := os.Args[1]
	var logs bytes.Buffer
	logger := log.New(&logs, "", 0)
	reporter := zipkinhttp.NewReporter(base+"/api/v2/spans", zipkinhttp.Logger(logger), zipkinhttp.Timeout(10*time.Second))
	endpoint, err := zipkin.NewEndpoint("go-frontend", "127.0.0.1:8080")
	if err != nil {
		panic(err)
	}
	tracer, err := zipkin.NewTracer(reporter, zipkin.WithLocalEndpoint(endpoint), zipkin.WithSharedSpans(false))
	if err != nil {
		panic(err)
	}
	root := tracer.StartSpan("checkout", zipkin.Kind(model.Server))
	root.Tag("http.method", "POST")
	root.Annotate(time.Now(), "cart-loaded")
	child := tracer.StartSpan("charge-card", zipkin.Kind(model.Client), zipkin.Parent(root.Context()),
		zipkin.RemoteEndpoint(&model.Endpoint{ServiceName: "payments", Port: 9000}))
	time.Sleep(2 * time.Millisecond)
	child.Finish()
	root.Finish()
	if err := reporter.Close(); err != nil {
		panic(err)
	}
	traceID := root.Context().TraceID.String()

	refusing := zipkinhttp.NewReporter(base+"/api/v2/spans", zipkinhttp.Logger(logger), zipkinhttp.Timeout(10*time.Second))
	refuser, _ := zipkin.NewTracer(refusing, zipkin.WithLocalEndpoint(endpoint))
	refuser.StartSpan("refuse-me").Finish()
	refusing.Close()

	var services []string
	servicesStatus, err := get(base, "/api/v2/services", &services)
	if err != nil {
		panic(fmt.Sprintf("services: %v", err))
	}
	var trace []model.SpanModel
	traceStatus, err := get(base, "/api/v2/trace/"+traceID, &trace)
	if err != nil {
		panic(fmt.Sprintf("trace: %v", err))
	}
	var missing []model.SpanModel
	missingStatus, _ := get(base, "/api/v2/trace/00000000000000ff", &missing)
	type decoded struct {
		Name     string            `json:"name"`
		Kind     string            `json:"kind"`
		Parent   string            `json:"parent"`
		Service  string            `json:"service"`
		Remote   string            `json:"remote"`
		Tags     map[string]string `json:"tags"`
		Notes    []string          `json:"annotations"`
		Duration int64             `json:"duration_us"`
		TraceID  string            `json:"trace_id"`
	}
	var spans []decoded
	for _, s := range trace {
		d := decoded{Name: s.Name, Kind: string(s.Kind), Tags: s.Tags, Duration: s.Duration.Microseconds(), TraceID: s.TraceID.String()}
		if s.ParentID != nil {
			d.Parent = s.ParentID.String()
		}
		if s.LocalEndpoint != nil {
			d.Service = s.LocalEndpoint.ServiceName
		}
		if s.RemoteEndpoint != nil {
			d.Remote = s.RemoteEndpoint.ServiceName
		}
		for _, a := range s.Annotations {
			d.Notes = append(d.Notes, a.Value)
		}
		spans = append(spans, d)
	}
	out, _ := json.Marshal(map[string]any{
		"trace_id":        traceID,
		"root_id":         root.Context().ID.String(),
		"services_status": servicesStatus,
		"services":        services,
		"trace_status":    traceStatus,
		"trace":           spans,
		"missing_status":  missingStatus,
		"reporter_log":    strings.TrimSpace(logs.String()),
	})
	fmt.Println(string(out))
}
