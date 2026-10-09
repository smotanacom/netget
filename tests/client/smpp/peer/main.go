// linxGnu/gosmpp, unchanged, as an ESME against NetGet's SMSC:
//
//	peer gosmpp ADDR SYSTEM_ID PASSWORD  binds as a transceiver, submits an ASCII message
//	                                     (asking for a receipt), a UCS-2 one and one the SMSC
//	                                     refuses, and prints one JSON line with every response
//	                                     and deliver_sm
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"sync"
	"time"

	"github.com/linxGnu/gosmpp"
	"github.com/linxGnu/gosmpp/data"
	"github.com/linxGnu/gosmpp/pdu"
)

func runGosmpp(addr, user, pass string) map[string]any {
	var mu sync.Mutex
	out := map[string]any{"responses": []any{}, "delivered": []any{}}
	done := make(chan struct{}, 8)
	session, err := gosmpp.NewSession(
		gosmpp.TRXConnector(gosmpp.NonTLSDialer, gosmpp.Auth{SMSC: addr, SystemID: user, Password: pass}),
		gosmpp.Settings{
			ReadTimeout: 30 * time.Second,
			OnReceivingError: func(err error) {
				mu.Lock()
				out["receiving_error"] = err.Error()
				mu.Unlock()
			},
			OnPDU: func(p pdu.PDU, responded bool) {
				mu.Lock()
				defer mu.Unlock()
				switch pd := p.(type) {
				case *pdu.SubmitSMResp:
					out["responses"] = append(out["responses"].([]any), map[string]any{"status": uint32(pd.CommandStatus), "message_id": pd.MessageID})
					done <- struct{}{}
				case *pdu.DeliverSM:
					text, _ := pd.Message.GetMessage()
					out["delivered"] = append(out["delivered"].([]any), map[string]any{"from": pd.SourceAddr.Address(), "to": pd.DestAddr.Address(), "esm_class": pd.EsmClass, "text": text, "acked": responded})
					done <- struct{}{}
				}
			},
		}, -1)
	if err != nil {
		return map[string]any{"fatal": err.Error()}
	}
	defer session.Close()
	submit := func(to, text string, enc data.Encoding, receipt byte) {
		src := pdu.NewAddress()
		_ = src.SetAddress("NetGetTest")
		dst := pdu.NewAddress()
		dst.SetTon(1)
		dst.SetNpi(1)
		_ = dst.SetAddress(to)
		sm := pdu.NewSubmitSM().(*pdu.SubmitSM)
		sm.SourceAddr = src
		sm.DestAddr = dst
		_ = sm.Message.SetMessageWithEncoding(text, enc)
		sm.RegisteredDelivery = receipt
		if err := session.Transceiver().Submit(sm); err != nil {
			mu.Lock()
			out["submit_error"] = err.Error()
			mu.Unlock()
		}
	}
	submit("15551230001", "hello from gosmpp", data.GSM7BIT, 1)
	submit("15551230002", "Привет", data.UCS2, 0)
	submit("44990000000", "rejected", data.GSM7BIT, 0)
	// Three responses, one receipt and two replies are expected; stop after a quiet second.
	for {
		select {
		case <-done:
		case <-time.After(1500 * time.Millisecond):
			mu.Lock()
			defer mu.Unlock()
			return out
		}
	}
}

func main() {
	if len(os.Args) < 4 {
		fmt.Fprintln(os.Stderr, "usage: peer gosmpp ADDR SYSTEM_ID PASSWORD")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "gosmpp":
		b, _ := json.Marshal(runGosmpp(os.Args[2], os.Args[3], os.Args[4]))
		fmt.Println(string(b))
	default:
		os.Exit(2)
	}
}
