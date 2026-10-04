// go-amqp v1.7.0, unchanged, as an AMQP 1.0 client of NetGet's container.
//
//	goamqp ADDR   SASL PLAIN alice/secret; a receiver on orders.confirmed; a sender on orders
//	    (an accepted order, a rejected one); a refused sender on forbidden; a refused password
//
// One JSON line per observation.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"time"

	"github.com/Azure/go-amqp"
)

func emit(v map[string]any) {
	b, _ := json.Marshal(v)
	fmt.Println(string(b))
}

func amqpErr(err error) map[string]any {
	var e *amqp.Error
	if errors.As(err, &e) {
		return map[string]any{"condition": string(e.Condition), "description": e.Description}
	}
	var le *amqp.LinkError
	if errors.As(err, &le) && le.RemoteErr != nil {
		return map[string]any{"condition": string(le.RemoteErr.Condition), "description": le.RemoteErr.Description}
	}
	return map[string]any{"error": err.Error()}
}

func main() {
	addr := "amqp://" + os.Args[1]
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	conn, err := amqp.Dial(ctx, addr, &amqp.ConnOptions{SASLType: amqp.SASLTypePlain("alice", "secret")})
	if err != nil {
		emit(map[string]any{"step": "dial", "error": err.Error()})
		os.Exit(1)
	}
	session, err := conn.NewSession(ctx, nil)
	if err != nil {
		emit(map[string]any{"step": "session", "error": err.Error()})
		os.Exit(1)
	}
	receiver, err := session.NewReceiver(ctx, "orders.confirmed", &amqp.ReceiverOptions{Credit: 10})
	if err != nil {
		emit(map[string]any{"step": "receiver", "error": err.Error()})
		os.Exit(1)
	}
	sender, err := session.NewSender(ctx, "orders", nil)
	if err != nil {
		emit(map[string]any{"step": "sender", "error": err.Error()})
		os.Exit(1)
	}
	err = sender.Send(ctx, amqp.NewMessage([]byte(`{"order": 2}`)), nil)
	emit(map[string]any{"step": "data_order", "result": fmt.Sprint(err)})
	msg := &amqp.Message{Value: map[string]any{"order": int64(3)}, Properties: &amqp.MessageProperties{Subject: ptr("go")}}
	err = sender.Send(ctx, msg, nil)
	if err != nil {
		out := amqpErr(err)
		out["step"] = "order"
		emit(out)
	} else {
		emit(map[string]any{"step": "order", "outcome": "accepted"})
	}
	err = sender.Send(ctx, &amqp.Message{Value: map[string]any{"nope": true}}, nil)
	out := amqpErr(err)
	out["step"] = "no_order"
	emit(out)
	rctx, rcancel := context.WithTimeout(ctx, 5*time.Second)
	got, err := receiver.Receive(rctx, nil)
	rcancel()
	if err != nil {
		emit(map[string]any{"step": "confirmation", "error": err.Error()})
	} else {
		_ = receiver.AcceptMessage(ctx, got)
		emit(map[string]any{"step": "confirmation", "value": got.Value, "subject": *got.Properties.Subject})
	}
	_, err = session.NewSender(ctx, "forbidden", nil)
	out = amqpErr(err)
	out["step"] = "forbidden"
	emit(out)
	_ = conn.Close()
	_, err = amqp.Dial(ctx, addr, &amqp.ConnOptions{SASLType: amqp.SASLTypePlain("alice", "wrong")})
	emit(map[string]any{"step": "bad_password", "error": fmt.Sprint(err)})
}

func ptr[T any](v T) *T { return &v }
