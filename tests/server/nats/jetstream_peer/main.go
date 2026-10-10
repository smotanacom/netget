// nats.go's jetstream package, unchanged, against a JetStream server: create a stream, publish
// three messages (and one the server refuses), create a durable pull consumer, fetch two,
// ack one asynchronously and one synchronously, fetch the rest without waiting, read stream and
// consumer info, list stream names, read account info, delete the consumer and the stream.
// Prints one JSON line.
//
// Usage: nats-jetstream-peer nats://host:port
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/nats-io/nats.go/jetstream"
)

func must[T any](v T, err error) T {
	if err != nil {
		panic(err)
	}
	return v
}

func main() {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	nc := must(nats.Connect(os.Args[1], nats.Timeout(10*time.Second)))
	defer nc.Close()
	js := must(jetstream.New(nc))
	out := map[string]any{}

	stream := must(js.CreateStream(ctx, jetstream.StreamConfig{Name: "ORDERS", Subjects: []string{"orders.*"}}))
	out["created"] = stream.CachedInfo().Config.Name
	out["created_subjects"] = stream.CachedInfo().Config.Subjects
	var seqs []uint64
	for i, body := range []string{`{"id":1}`, `{"id":2}`, `{"id":3}`} {
		msg := nats.NewMsg(fmt.Sprintf("orders.new"))
		msg.Data = []byte(body)
		msg.Header.Set("Order-Index", fmt.Sprint(i+1))
		ack := must(js.PublishMsg(ctx, msg))
		if ack.Stream != "ORDERS" {
			panic("ack names stream " + ack.Stream)
		}
		seqs = append(seqs, ack.Sequence)
	}
	out["published_seqs"] = seqs
	if _, err := js.Publish(ctx, "orders.rejected", []byte("no")); err != nil {
		out["refused"] = err.Error()
	}

	cons := must(stream.CreateOrUpdateConsumer(ctx, jetstream.ConsumerConfig{Durable: "proc", AckPolicy: jetstream.AckExplicitPolicy}))
	out["consumer"] = cons.CachedInfo().Name
	batch := must(cons.Fetch(2, jetstream.FetchMaxWait(5*time.Second)))
	var got []map[string]any
	i := 0
	for msg := range batch.Messages() {
		meta := must(msg.Metadata())
		got = append(got, map[string]any{"subject": msg.Subject(), "data": string(msg.Data()),
			"header": msg.Headers().Get("Order-Index"), "stream_seq": meta.Sequence.Stream,
			"consumer_seq": meta.Sequence.Consumer, "stream": meta.Stream, "consumer": meta.Consumer})
		if i == 0 {
			if err := msg.Ack(); err != nil {
				panic(err)
			}
		} else if err := msg.DoubleAck(ctx); err != nil {
			panic(err)
		}
		i++
	}
	if batch.Error() != nil {
		out["batch_error"] = batch.Error().Error()
	}
	out["fetched"] = got
	rest := must(cons.FetchNoWait(5))
	var tail []string
	for msg := range rest.Messages() {
		tail = append(tail, string(msg.Data()))
		_ = msg.Ack()
	}
	out["rest"] = tail
	empty := must(cons.FetchNoWait(5))
	n := 0
	for range empty.Messages() {
		n++
	}
	out["empty_count"] = n

	info := must(stream.Info(ctx))
	out["stream_messages"] = info.State.Msgs
	out["stream_last_seq"] = info.State.LastSeq
	ci := must(cons.Info(ctx))
	out["ack_floor"] = ci.AckFloor.Stream
	var names []string
	lister := js.StreamNames(ctx)
	for name := range lister.Name() {
		names = append(names, name)
	}
	out["names"] = names
	acct := must(js.AccountInfo(ctx))
	out["account_streams"] = acct.Streams
	if _, err := js.Stream(ctx, "MISSING"); err != nil {
		out["missing"] = err.Error()
	}
	must(0, stream.DeleteConsumer(ctx, "proc"))
	must(0, js.DeleteStream(ctx, "ORDERS"))
	b, _ := json.Marshal(out)
	fmt.Println(string(b))
}
