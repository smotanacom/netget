// go-zeromq/zmq4, a pure-Go ZMTP implementation, unchanged, driven against NetGet:
//
//	peer req ADDR FRAME...   one request; prints {"reply": [...]}
//	peer push ADDR MSG...    one single-frame message per MSG; prints {"pushed": N}
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"time"

	"github.com/go-zeromq/zmq4"
)

func fail(err error) {
	b, _ := json.Marshal(map[string]string{"error": err.Error()})
	fmt.Println(string(b))
	os.Exit(1)
}

func main() {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	if len(os.Args) < 3 {
		fail(fmt.Errorf("usage: peer req|push ADDR ..."))
	}
	addr := "tcp://" + os.Args[2]
	switch os.Args[1] {
	case "req":
		s := zmq4.NewReq(ctx)
		defer s.Close()
		if err := s.Dial(addr); err != nil {
			fail(err)
		}
		frames := [][]byte{}
		for _, f := range os.Args[3:] {
			frames = append(frames, []byte(f))
		}
		if err := s.Send(zmq4.NewMsgFrom(frames...)); err != nil {
			fail(err)
		}
		msg, err := s.Recv()
		if err != nil {
			fail(err)
		}
		reply := []string{}
		for _, f := range msg.Frames {
			reply = append(reply, string(f))
		}
		b, _ := json.Marshal(map[string]any{"reply": reply})
		fmt.Println(string(b))
	case "push":
		s := zmq4.NewPush(ctx)
		defer s.Close()
		if err := s.Dial(addr); err != nil {
			fail(err)
		}
		for _, m := range os.Args[3:] {
			if err := s.Send(zmq4.NewMsgString(m)); err != nil {
				fail(err)
			}
		}
		// zmq4 has no linger: give the frames time to leave before closing.
		time.Sleep(500 * time.Millisecond)
		b, _ := json.Marshal(map[string]any{"pushed": len(os.Args) - 3})
		fmt.Println(string(b))
	default:
		fail(fmt.Errorf("unknown mode %s", os.Args[1]))
	}
}
