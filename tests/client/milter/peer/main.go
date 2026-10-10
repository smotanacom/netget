// emersion/go-milter, unchanged, in either role. Prints one JSON line (client) or READY <addr>.
//
//	milter-peer client HOST:PORT   the MTA side: a clean message, a spam recipient, a
//	                               refused sender, against a filter
//	milter-peer server HOST:PORT   a filter listening there: rejects spam@… recipients, answers
//	                               550 to senders containing "spammer", and at end of message
//	                               adds X-Go-Milter: seen, adds <audit@example.com> and
//	                               prefixes the Subject with [go]
package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"strings"

	"github.com/emersion/go-milter"
)

func act(a *milter.Action) string {
	switch a.Code {
	case milter.ActContinue:
		return "continue"
	case milter.ActAccept:
		return "accept"
	case milter.ActReject:
		return "reject"
	case milter.ActTempFail:
		return "tempfail"
	case milter.ActDiscard:
		return "discard"
	case milter.ActReplyCode:
		return fmt.Sprintf("reply %d %s", a.SMTPCode, a.SMTPText)
	}
	return "other"
}

func must[T any](v T, err error) T {
	if err != nil {
		panic(err)
	}
	return v
}

func client(addr string) map[string]any {
	c := milter.NewClientWithOptions("tcp", addr, milter.ClientOptions{
		ActionMask:   milter.OptAddHeader | milter.OptChangeHeader | milter.OptAddRcpt | milter.OptRemoveRcpt | milter.OptChangeBody | milter.OptQuarantine,
		ProtocolMask: 0,
	})
	defer c.Close()
	out := map[string]any{}
	s := must(c.Session())
	out["connect"] = act(must(s.Conn("client.example", milter.FamilyInet, 4000, "192.0.2.20")))
	out["helo"] = act(must(s.Helo("client.example")))
	out["mail"] = act(must(s.Mail("alice@example.com", nil)))
	out["rcpt_spam"] = act(must(s.Rcpt("spam@example.net", nil)))
	out["rcpt"] = act(must(s.Rcpt("bob@example.net", nil)))
	out["header"] = act(must(s.HeaderField("Subject", "hello")))
	out["eoh"] = act(must(s.HeaderEnd()))
	mods, final := must2(s.BodyReadFrom(strings.NewReader("Hi Bob\r\n")))
	out["final"] = act(final)
	var ms []string
	for _, m := range mods {
		switch m.Code {
		case milter.ActAddHeader:
			ms = append(ms, "add_header "+m.HeaderName+": "+m.HeaderValue)
		case milter.ActChangeHeader:
			ms = append(ms, fmt.Sprintf("change_header %s[%d]: %s", m.HeaderName, m.HeaderIndex, m.HeaderValue))
		case milter.ActAddRcpt:
			ms = append(ms, "add_rcpt "+m.Rcpt)
		case milter.ActQuarantine:
			ms = append(ms, "quarantine "+m.Reason)
		default:
			ms = append(ms, fmt.Sprintf("other %c", m.Code))
		}
	}
	out["modifications"] = ms
	s.Close()
	s2 := must(c.Session())
	must(s2.Conn("client.example", milter.FamilyInet, 4001, "192.0.2.21"))
	out["mail_spammer"] = act(must(s2.Mail("spammer@bad.example", nil)))
	s2.Close()
	return out
}

func must2[A, B any](a A, b B, err error) (A, B) {
	if err != nil {
		panic(err)
	}
	return a, b
}

type filter struct {
	milter.NoOpMilter
	subject string
}

func (f *filter) MailFrom(from string, m *milter.Modifier) (milter.Response, error) {
	if strings.Contains(from, "spammer") {
		return milter.NewResponseStr('y', "550 5.7.1 go-milter refuses spammers"), nil
	}
	return milter.RespContinue, nil
}

func (f *filter) RcptTo(rcpt string, m *milter.Modifier) (milter.Response, error) {
	if strings.HasPrefix(rcpt, "spam@") {
		return milter.RespReject, nil
	}
	return milter.RespContinue, nil
}

func (f *filter) Header(name, value string, m *milter.Modifier) (milter.Response, error) {
	if strings.EqualFold(name, "Subject") {
		f.subject = value
	}
	return milter.RespContinue, nil
}

func (f *filter) Body(m *milter.Modifier) (milter.Response, error) {
	if err := m.AddHeader("X-Go-Milter", "seen"); err != nil {
		return nil, err
	}
	if err := m.AddRecipient("audit@example.com"); err != nil {
		return nil, err
	}
	if err := m.ChangeHeader(1, "Subject", "[go] "+f.subject); err != nil {
		return nil, err
	}
	return milter.RespAccept, nil
}

func serve(addr string) {
	l := must(net.Listen("tcp", addr))
	fmt.Println("READY", l.Addr().String())
	s := milter.Server{
		NewMilter: func() milter.Milter { return &filter{} },
		Actions:   milter.OptAddHeader | milter.OptAddRcpt | milter.OptChangeHeader,
	}
	if err := s.Serve(l); err != nil && err != io.EOF {
		panic(err)
	}
}

func main() {
	if os.Args[1] == "server" {
		serve(os.Args[2])
		return
	}
	b, _ := json.Marshal(client(os.Args[2]))
	fmt.Println(string(b))
}
