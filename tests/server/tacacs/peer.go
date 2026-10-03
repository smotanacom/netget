package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	t "github.com/nwaples/tacplus"
	"net"
	"os"
	"time"
)

type handler struct{}

func (handler) HandleAuthenStart(ctx context.Context, a *t.AuthenStart, s *t.ServerSession) *t.AuthenReply {
	user := a.User
	pwd := string(a.Data)
	if a.Action != t.AuthenActionLogin {
		return &t.AuthenReply{Status: t.AuthenStatusFail}
	}
	if a.AuthenType == t.AuthenTypeASCII {
		if user == "" {
			v, e := s.GetUser(ctx, "Username:")
			if e != nil {
				return nil
			}
			user = v.Message
		}
		v, e := s.GetPass(ctx, "Password:")
		if e != nil {
			return nil
		}
		pwd = v.Message
	} else if a.AuthenType != t.AuthenTypePAP {
		return &t.AuthenReply{Status: t.AuthenStatusFail}
	}
	status := uint8(t.AuthenStatusFail)
	if user == "alice" && pwd == "correct" {
		status = t.AuthenStatusPass
	}
	return &t.AuthenReply{Status: status, ServerMsg: "peer authentication"}
}
func (handler) HandleAuthorRequest(ctx context.Context, a *t.AuthorRequest, s *t.ServerSession) *t.AuthorResponse {
	status := uint8(t.AuthorStatusFail)
	if a.User == "alice" {
		status = t.AuthorStatusPassAdd
	}
	return &t.AuthorResponse{Status: status, Arg: []string{"priv-lvl=15", "service=shell", "audit*enabled"}, ServerMsg: "peer authorization", Data: "observed"}
}
func (handler) HandleAcctRequest(ctx context.Context, a *t.AcctRequest, s *t.ServerSession) *t.AcctReply {
	emit(map[string]interface{}{"recorded": true, "username": a.User, "record_type": a.Flags, "arguments": a.Arg, "method": a.AuthenMethod, "privilege_level": a.PrivLvl, "authentication_type": a.AuthenType, "service": a.AuthenService, "port": a.Port, "remote_address": a.RemAddr})
	return &t.AcctReply{Status: t.AcctStatusSuccess, ServerMsg: "peer accounting", Data: "observed"}
}
func emit(v interface{}) {
	if e := json.NewEncoder(os.Stdout).Encode(v); e != nil {
		panic(e)
	}
}
func check(e error) {
	if e != nil {
		fmt.Fprintln(os.Stderr, e)
		os.Exit(1)
	}
}
func main() {
	version := flag.Bool("version", false, "")
	role := flag.String("role", "client", "")
	addr := flag.String("addr", "127.0.0.1:0", "")
	secret := flag.String("secret", "test-secret", "")
	flag.Parse()
	if *version {
		emit(map[string]interface{}{"peer": "nwaples/tacplus", "version": "0.0.3", "revision": "01141c615540e7ae8bf5ca1b412d0788cb34222b", "source_modified": false})
		return
	}
	config := t.ConnConfig{Secret: []byte(*secret), ReadTimeout: 5 * time.Second, WriteTimeout: 5 * time.Second, IdleTimeout: 5 * time.Second}
	if *role == "server" {
		l, e := net.Listen("tcp", *addr)
		check(e)
		emit(map[string]interface{}{"ready": l.Addr().String()})
		h := t.ServerConnHandler{Handler: handler{}, ConnConfig: config}
		s := t.Server{ServeConn: h.Serve}
		check(s.Serve(l))
		return
	}
	c := t.Client{Addr: *addr, ConnConfig: config}
	defer c.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	for _, kind := range []uint8{t.AuthenTypeASCII, t.AuthenTypePAP} {
		a := &t.AuthenStart{Action: t.AuthenActionLogin, PrivLvl: 1, AuthenType: kind, AuthenService: t.AuthenServiceLogin, User: "alice", Port: "tty1", RemAddr: "192.0.2.10"}
		if kind == t.AuthenTypePAP {
			a.Data = []byte("correct")
		}
		r, s, e := c.SendAuthenStart(ctx, a)
		check(e)
		if r.Status == t.AuthenStatusGetPass {
			r, e = s.Continue(ctx, "correct")
			check(e)
		}
		emit(map[string]interface{}{"role": "authentication", "kind": kind, "status": r.Status, "server_message": r.ServerMsg})
		if r.Status != t.AuthenStatusPass {
			os.Exit(2)
		}
	}
	a, e := c.SendAuthorRequest(ctx, &t.AuthorRequest{AuthenMethod: t.AuthenMethodTACACSPlus, PrivLvl: 1, AuthenType: t.AuthenTypeASCII, AuthenService: t.AuthenServiceLogin, User: "alice", Port: "tty1", RemAddr: "192.0.2.10", Arg: []string{"service=shell", "cmd=show", "cmd-arg=version"}})
	check(e)
	emit(a)
	if a.Status != t.AuthorStatusPassAdd {
		os.Exit(3)
	}
	b, e := c.SendAcctRequest(ctx, &t.AcctRequest{Flags: t.AcctFlagStart, AuthenMethod: t.AuthenMethodTACACSPlus, PrivLvl: 1, AuthenType: t.AuthenTypeASCII, AuthenService: t.AuthenServiceLogin, User: "alice", Arg: []string{"task_id=42", "start_time=1700000000"}})
	check(e)
	emit(b)
	if b.Status != t.AcctStatusSuccess {
		os.Exit(4)
	}
}
