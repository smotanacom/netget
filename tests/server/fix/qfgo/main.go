// QuickFIX/Go v0.9.12, unchanged, as the independent FIX 4.4 peer for NetGet's tests. Every
// application message is validated against the module's own FIX44.xml data dictionary.
//
//	qfgo initiator HOST PORT SENDER DICTIONARY   logs on to NetGet's acceptor, sends a
//	    NewOrderSingle for AAPL and one for REJECT, waits for the answers and three
//	    heartbeats, and logs out
//	qfgo acceptor PORT DICTIONARY                accepts CLIENT -> QFGO sessions, answers each
//	    NewOrderSingle with an ExecutionReport and an OrderCancelRequest with a
//	    BusinessMessageReject
//
// One JSON line per message received ({"dir":"app"|"admin","type":..,"fields":{tag:value}})
// and per session event ({"event":"logon"|"logout"}).
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/quickfixgo/quickfix"
	"github.com/quickfixgo/quickfix/log/screen"
)

var printMu sync.Mutex

func emit(v map[string]any) {
	printMu.Lock()
	defer printMu.Unlock()
	b, _ := json.Marshal(v)
	fmt.Println(string(b))
}

func fields(m *quickfix.Message) map[string]string {
	out := map[string]string{}
	for _, fm := range []*quickfix.FieldMap{&m.Header.FieldMap, &m.Body.FieldMap} {
		for _, t := range fm.Tags() {
			if v, err := fm.GetString(t); err == nil {
				out[strconv.Itoa(int(t))] = v
			}
		}
	}
	return out
}

type app struct {
	mode     string
	logon    chan struct{}
	answers  chan string
	loggedOn bool
}

func (a *app) OnCreate(quickfix.SessionID) {}
func (a *app) OnLogon(quickfix.SessionID) {
	emit(map[string]any{"event": "logon"})
	a.loggedOn = true
	select {
	case a.logon <- struct{}{}:
	default:
	}
}
func (a *app) OnLogout(quickfix.SessionID)                       { emit(map[string]any{"event": "logout"}) }
func (a *app) ToAdmin(*quickfix.Message, quickfix.SessionID)     {}
func (a *app) ToApp(*quickfix.Message, quickfix.SessionID) error { return nil }
func (a *app) FromAdmin(m *quickfix.Message, _ quickfix.SessionID) quickfix.MessageRejectError {
	t, _ := m.MsgType()
	emit(map[string]any{"dir": "admin", "type": t, "fields": fields(m)})
	return nil
}
func (a *app) FromApp(m *quickfix.Message, id quickfix.SessionID) quickfix.MessageRejectError {
	t, _ := m.MsgType()
	f := fields(m)
	emit(map[string]any{"dir": "app", "type": t, "fields": f})
	if a.mode == "acceptor" {
		switch t {
		case "D":
			r := quickfix.NewMessage()
			r.Header.SetString(35, "8")
			r.Body.SetString(37, "QF-"+f["11"])
			r.Body.SetString(11, f["11"])
			r.Body.SetString(17, "EX-"+f["11"])
			r.Body.SetString(150, "0")
			r.Body.SetString(39, "0")
			r.Body.SetString(55, f["55"])
			r.Body.SetString(54, f["54"])
			r.Body.SetString(151, f["38"])
			r.Body.SetString(14, "0")
			r.Body.SetString(6, "0")
			_ = quickfix.SendToTarget(r, id)
		case "F":
			r := quickfix.NewMessage()
			r.Header.SetString(35, "j")
			r.Body.SetString(45, f["34"])
			r.Body.SetString(372, "F")
			r.Body.SetString(380, "3")
			r.Body.SetString(58, "cancels are not supported here")
			_ = quickfix.SendToTarget(r, id)
		}
	} else {
		select {
		case a.answers <- t:
		default:
		}
	}
	return nil
}

func settings(text string) *quickfix.Settings {
	s, err := quickfix.ParseSettings(strings.NewReader(text))
	if err != nil {
		panic(err)
	}
	return s
}

func order(id, symbol string) *quickfix.Message {
	m := quickfix.NewMessage()
	m.Header.SetString(35, "D")
	m.Body.SetString(11, id)
	m.Body.SetString(21, "1")
	m.Body.SetString(55, symbol)
	m.Body.SetString(54, "1")
	m.Body.SetString(60, time.Now().UTC().Format("20060102-15:04:05.000"))
	m.Body.SetString(38, "100")
	m.Body.SetString(40, "2")
	m.Body.SetString(44, "150.25")
	return m
}

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: qfgo initiator HOST PORT SENDER DICTIONARY | qfgo acceptor PORT DICTIONARY")
		os.Exit(2)
	}
	a := &app{mode: os.Args[1], logon: make(chan struct{}, 1), answers: make(chan string, 8)}
	switch os.Args[1] {
	case "initiator":
		host, port, sender, dict := os.Args[2], os.Args[3], os.Args[4], os.Args[5]
		// A refused Logon reaches no callback, so the wire is printed too (lines not JSON).
		logs := screen.NewLogFactory()
		cfg := settings(fmt.Sprintf("[DEFAULT]\nConnectionType=initiator\nSocketConnectHost=%s\nSocketConnectPort=%s\nHeartBtInt=1\nReconnectInterval=60\nResetOnLogon=Y\nDataDictionary=%s\n[SESSION]\nBeginString=FIX.4.4\nSenderCompID=%s\nTargetCompID=NETGET\n", host, port, dict, sender))
		init, err := quickfix.NewInitiator(a, quickfix.NewMemoryStoreFactory(), cfg, logs)
		if err != nil {
			panic(err)
		}
		if err := init.Start(); err != nil {
			panic(err)
		}
		id := quickfix.SessionID{BeginString: "FIX.4.4", SenderCompID: sender, TargetCompID: "NETGET"}
		select {
		case <-a.logon:
		case <-time.After(10 * time.Second):
			emit(map[string]any{"event": "no_logon"})
			init.Stop()
			return
		}
		for _, o := range []*quickfix.Message{order("1", "AAPL"), order("2", "REJECT")} {
			if err := quickfix.SendToTarget(o, id); err != nil {
				panic(err)
			}
			select {
			case <-a.answers:
			case <-time.After(10 * time.Second):
				emit(map[string]any{"event": "no_answer"})
			}
		}
		time.Sleep(3500 * time.Millisecond)
		init.Stop()
	case "acceptor":
		port, dict := os.Args[2], os.Args[3]
		cfg := settings(fmt.Sprintf("[DEFAULT]\nConnectionType=acceptor\nSocketAcceptHost=127.0.0.1\nSocketAcceptPort=%s\nHeartBtInt=30\nDataDictionary=%s\n[SESSION]\nBeginString=FIX.4.4\nSenderCompID=QFGO\nTargetCompID=CLIENT\n", port, dict))
		acc, err := quickfix.NewAcceptor(a, quickfix.NewMemoryStoreFactory(), cfg, quickfix.NewNullLogFactory())
		if err != nil {
			panic(err)
		}
		if err := acc.Start(); err != nil {
			panic(err)
		}
		emit(map[string]any{"event": "listening", "port": port})
		select {}
	}
}
