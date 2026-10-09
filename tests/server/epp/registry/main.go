// A small registry over the Swedish Internet Foundation's epp-lib v0.2.0 (unchanged), the
// independent EPP server NetGet's client is tested against. epp-lib owns TLS, RFC 5734
// framing, the greeting call and command routing by namespace URI (a command in the wrong
// namespace matches nothing and the connection is closed); this file owns the registry.
//
//	registry DIR    writes DIR/cert.pem and prints {"ready":true,"port":N}
package main

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"html"
	"io"
	"log/slog"
	"math/big"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/beevik/etree"
	epplib "github.com/dotse/epp-lib"
)

const (
	nsEPP     = "urn:ietf:params:xml:ns:epp-1.0"
	nsDomain  = "urn:ietf:params:xml:ns:domain-1.0"
	nsHost    = "urn:ietf:params:xml:ns:host-1.0"
	nsContact = "urn:ietf:params:xml:ns:contact-1.0"
	stamp     = "2006-01-02T15:04:05.0Z"
)

type domain struct {
	registrant, clID, authInfo string
	contacts                   [][2]string
	ns                         []string
	crDate, exDate             time.Time
	transfer                   string
	transferTo                 string
}

var (
	mu       sync.Mutex
	domains  = map[string]*domain{}
	hosts    = map[string][]string{}
	contacts = map[string]string{}
	serial   atomic.Int64
)

type session struct{ client string }
type sessionKey struct{}

func sess(ctx context.Context) *session { return ctx.Value(sessionKey{}).(*session) }

func x(s string) string { return html.EscapeString(s) }

func path(parts ...string) string {
	b := epplib.NewXMLPathBuilder()
	for i := 0; i+1 < len(parts); i += 2 {
		if i == 0 {
			b = b.AddOrphan(parts[i], parts[i+1])
		} else {
			b = b.Add(parts[i], parts[i+1])
		}
	}
	return b.String()
}

func texts(doc *etree.Document, tag, ns string) []string {
	var out []string
	for _, e := range doc.FindElements(path("//"+tag, ns)) {
		out = append(out, strings.TrimSpace(e.Text()))
	}
	return out
}

func text(doc *etree.Document, tag, ns string) string {
	if t := texts(doc, tag, ns); len(t) > 0 {
		return t[0]
	}
	return ""
}

func respond(w epplib.Writer, doc *etree.Document, code int, msg, resData string) {
	// Handlers call this holding mu, so the counter has its own synchronisation.
	sv := fmt.Sprintf("REG-%d", serial.Add(1))
	cl := ""
	if doc != nil {
		if t := text(doc, "clTRID", nsEPP); t != "" {
			cl = "<clTRID>" + x(t) + "</clTRID>"
		}
	}
	if resData != "" {
		resData = "<resData>" + resData + "</resData>"
	}
	fmt.Fprintf(w, `<?xml version="1.0" encoding="UTF-8" standalone="no"?><epp xmlns="%s"><response><result code="%d"><msg>%s</msg></result>%s<trID>%s<svTRID>%s</svTRID></trID></response></epp>`, nsEPP, code, x(msg), resData, cl, sv)
}

func loggedIn(ctx context.Context, w epplib.Writer, doc *etree.Document) bool {
	if sess(ctx).client == "" {
		respond(w, doc, 2002, "Command use error", "")
		return false
	}
	return true
}

func greeting(_ context.Context, w epplib.Writer, _ *etree.Document) {
	fmt.Fprintf(w, `<?xml version="1.0" encoding="UTF-8" standalone="no"?><epp xmlns="%s"><greeting><svID>epp-lib test registry</svID><svDate>%s</svDate><svcMenu><version>1.0</version><lang>en</lang><objURI>%s</objURI><objURI>%s</objURI><objURI>%s</objURI></svcMenu><dcp><access><all/></access><statement><purpose><admin/><prov/></purpose><recipient><ours/></recipient><retention><stated/></retention></statement></dcp></greeting></epp>`, nsEPP, time.Now().UTC().Format(stamp), nsDomain, nsHost, nsContact)
}

func main() {
	dir := os.Args[1]
	key, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	tmpl := &x509.Certificate{
		SerialNumber: big.NewInt(time.Now().UnixNano()),
		Subject:      pkix.Name{CommonName: "localhost"},
		NotBefore:    time.Now().Add(-time.Minute),
		NotAfter:     time.Now().Add(7 * 24 * time.Hour),
		DNSNames:     []string{"localhost"},
		IPAddresses:  []net.IP{net.ParseIP("127.0.0.1")},
		KeyUsage:     x509.KeyUsageDigitalSignature,
		ExtKeyUsage:  []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	if err != nil {
		panic(err)
	}
	certPEM := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
	if err := os.WriteFile(filepath.Join(dir, "cert.pem"), certPEM, 0o600); err != nil {
		panic(err)
	}
	keyDER, _ := x509.MarshalECPrivateKey(key)
	pair, err := tls.X509KeyPair(certPEM, pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: keyDER}))
	if err != nil {
		panic(err)
	}

	// Seeded: a domain held by another registrar, its contact and host.
	contacts["other-1"] = "OtherReg"
	hosts["ns1.other.example"] = nil
	domains["taken.example"] = &domain{registrant: "other-1", clID: "OtherReg", authInfo: "move-me-1", ns: []string{"ns1.other.example"}, crDate: time.Date(2020, 1, 2, 3, 4, 5, 0, time.UTC), exDate: time.Date(2030, 1, 2, 3, 4, 5, 0, time.UTC)}

	mux := &epplib.CommandMux{}
	mux.BindGreeting(greeting)
	mux.Bind(path("//hello", nsEPP), greeting)
	mux.Bind(path("//command", nsEPP, "login", nsEPP), func(ctx context.Context, w epplib.Writer, doc *etree.Document) {
		if text(doc, "clID", nsEPP) == "registrar1" && text(doc, "pw", nsEPP) == "secret-pw-1" && text(doc, "version", nsEPP) == "1.0" {
			sess(ctx).client = "registrar1"
			respond(w, doc, 1000, "Command completed successfully", "")
			return
		}
		respond(w, doc, 2200, "Authentication error", "")
	})
	mux.Bind(path("//command", nsEPP, "logout", nsEPP), func(_ context.Context, w epplib.Writer, doc *etree.Document) {
		respond(w, doc, 1500, "Command completed successfully; ending session", "")
		w.CloseAfterWrite()
	})
	mux.BindCommand("check", nsDomain, func(ctx context.Context, w epplib.Writer, doc *etree.Document) {
		if !loggedIn(ctx, w, doc) {
			return
		}
		mu.Lock()
		defer mu.Unlock()
		cds := ""
		for _, n := range texts(doc, "name", nsDomain) {
			if _, taken := domains[strings.ToLower(n)]; taken {
				cds += fmt.Sprintf(`<domain:cd><domain:name avail="0">%s</domain:name><domain:reason>In use</domain:reason></domain:cd>`, x(n))
			} else {
				cds += fmt.Sprintf(`<domain:cd><domain:name avail="1">%s</domain:name></domain:cd>`, x(n))
			}
		}
		respond(w, doc, 1000, "Command completed successfully", `<domain:chkData xmlns:domain="`+nsDomain+`">`+cds+`</domain:chkData>`)
	})
	mux.BindCommand("create", nsContact, func(ctx context.Context, w epplib.Writer, doc *etree.Document) {
		if !loggedIn(ctx, w, doc) {
			return
		}
		mu.Lock()
		defer mu.Unlock()
		id := text(doc, "id", nsContact)
		if text(doc, "email", nsContact) == "" || text(doc, "cc", nsContact) == "" {
			respond(w, doc, 2003, "Required parameter missing", "")
			return
		}
		if _, ok := contacts[id]; ok {
			respond(w, doc, 2302, "Object exists", "")
			return
		}
		contacts[id] = sess(ctx).client
		respond(w, doc, 1000, "Command completed successfully", fmt.Sprintf(`<contact:creData xmlns:contact="%s"><contact:id>%s</contact:id><contact:crDate>%s</contact:crDate></contact:creData>`, nsContact, x(id), time.Now().UTC().Format(stamp)))
	})
	mux.BindCommand("create", nsHost, func(ctx context.Context, w epplib.Writer, doc *etree.Document) {
		if !loggedIn(ctx, w, doc) {
			return
		}
		mu.Lock()
		defer mu.Unlock()
		name := strings.ToLower(text(doc, "name", nsHost))
		if _, ok := hosts[name]; ok {
			respond(w, doc, 2302, "Object exists", "")
			return
		}
		hosts[name] = texts(doc, "addr", nsHost)
		respond(w, doc, 1000, "Command completed successfully", fmt.Sprintf(`<host:creData xmlns:host="%s"><host:name>%s</host:name><host:crDate>%s</host:crDate></host:creData>`, nsHost, x(name), time.Now().UTC().Format(stamp)))
	})
	mux.BindCommand("create", nsDomain, func(ctx context.Context, w epplib.Writer, doc *etree.Document) {
		if !loggedIn(ctx, w, doc) {
			return
		}
		mu.Lock()
		defer mu.Unlock()
		name := strings.ToLower(text(doc, "name", nsDomain))
		if _, ok := domains[name]; ok {
			respond(w, doc, 2302, "Object exists", "")
			return
		}
		registrant := text(doc, "registrant", nsDomain)
		if _, ok := contacts[registrant]; !ok {
			respond(w, doc, 2303, "Object does not exist", "")
			return
		}
		ns := texts(doc, "hostObj", nsDomain)
		for _, h := range ns {
			if _, ok := hosts[strings.ToLower(h)]; !ok {
				respond(w, doc, 2303, "Object does not exist", "")
				return
			}
		}
		years := 1
		if p := text(doc, "period", nsDomain); p != "" {
			years, _ = strconv.Atoi(p)
		}
		d := &domain{registrant: registrant, clID: sess(ctx).client, authInfo: text(doc, "pw", nsDomain), ns: ns, crDate: time.Now().UTC().Truncate(time.Second)}
		d.exDate = d.crDate.AddDate(years, 0, 0)
		for _, c := range doc.FindElements(path("//contact", nsDomain)) {
			d.contacts = append(d.contacts, [2]string{c.SelectAttrValue("type", ""), strings.TrimSpace(c.Text())})
		}
		domains[name] = d
		respond(w, doc, 1000, "Command completed successfully", fmt.Sprintf(`<domain:creData xmlns:domain="%s"><domain:name>%s</domain:name><domain:crDate>%s</domain:crDate><domain:exDate>%s</domain:exDate></domain:creData>`, nsDomain, x(name), d.crDate.Format(stamp), d.exDate.Format(stamp)))
	})
	mux.BindCommand("info", nsDomain, func(ctx context.Context, w epplib.Writer, doc *etree.Document) {
		if !loggedIn(ctx, w, doc) {
			return
		}
		mu.Lock()
		defer mu.Unlock()
		name := strings.ToLower(text(doc, "name", nsDomain))
		d, ok := domains[name]
		if !ok {
			respond(w, doc, 2303, "Object does not exist", "")
			return
		}
		body := fmt.Sprintf(`<domain:name>%s</domain:name><domain:roid>%s-REG</domain:roid><domain:status s="ok"/><domain:registrant>%s</domain:registrant>`, x(name), strings.ToUpper(strings.ReplaceAll(name, ".", "")), x(d.registrant))
		for _, c := range d.contacts {
			body += fmt.Sprintf(`<domain:contact type="%s">%s</domain:contact>`, x(c[0]), x(c[1]))
		}
		if len(d.ns) > 0 {
			body += "<domain:ns>"
			for _, h := range d.ns {
				body += "<domain:hostObj>" + x(h) + "</domain:hostObj>"
			}
			body += "</domain:ns>"
		}
		body += fmt.Sprintf(`<domain:clID>%s</domain:clID><domain:crDate>%s</domain:crDate><domain:exDate>%s</domain:exDate>`, x(d.clID), d.crDate.Format(stamp), d.exDate.Format(stamp))
		if d.clID == sess(ctx).client {
			body += "<domain:authInfo><domain:pw>" + x(d.authInfo) + "</domain:pw></domain:authInfo>"
		}
		respond(w, doc, 1000, "Command completed successfully", `<domain:infData xmlns:domain="`+nsDomain+`">`+body+`</domain:infData>`)
	})
	mux.BindCommand("renew", nsDomain, func(ctx context.Context, w epplib.Writer, doc *etree.Document) {
		if !loggedIn(ctx, w, doc) {
			return
		}
		mu.Lock()
		defer mu.Unlock()
		name := strings.ToLower(text(doc, "name", nsDomain))
		d, ok := domains[name]
		if !ok {
			respond(w, doc, 2303, "Object does not exist", "")
			return
		}
		if d.clID != sess(ctx).client {
			respond(w, doc, 2201, "Authorization error", "")
			return
		}
		if text(doc, "curExpDate", nsDomain) != d.exDate.Format("2006-01-02") {
			respond(w, doc, 2306, "Parameter value policy error", "")
			return
		}
		years := 1
		if p := text(doc, "period", nsDomain); p != "" {
			years, _ = strconv.Atoi(p)
		}
		d.exDate = d.exDate.AddDate(years, 0, 0)
		respond(w, doc, 1000, "Command completed successfully", fmt.Sprintf(`<domain:renData xmlns:domain="%s"><domain:name>%s</domain:name><domain:exDate>%s</domain:exDate></domain:renData>`, nsDomain, x(name), d.exDate.Format(stamp)))
	})
	mux.BindCommand("transfer", nsDomain, func(ctx context.Context, w epplib.Writer, doc *etree.Document) {
		if !loggedIn(ctx, w, doc) {
			return
		}
		mu.Lock()
		defer mu.Unlock()
		name := strings.ToLower(text(doc, "name", nsDomain))
		d, ok := domains[name]
		if !ok {
			respond(w, doc, 2303, "Object does not exist", "")
			return
		}
		op := doc.FindElement(path("//command", nsEPP, "transfer", nsEPP)).SelectAttrValue("op", "")
		code := 1000
		switch op {
		case "request":
			if text(doc, "pw", nsDomain) != d.authInfo {
				respond(w, doc, 2202, "Invalid authorization information", "")
				return
			}
			if d.transfer == "pending" {
				respond(w, doc, 2300, "Object pending transfer", "")
				return
			}
			d.transfer, d.transferTo, code = "pending", sess(ctx).client, 1001
		case "query":
			if d.transfer == "" {
				respond(w, doc, 2301, "Object not pending transfer", "")
				return
			}
		default:
			respond(w, doc, 2102, "Unimplemented option", "")
			return
		}
		now := time.Now().UTC().Truncate(time.Second)
		respond(w, doc, code, "Command completed successfully", fmt.Sprintf(`<domain:trnData xmlns:domain="%s"><domain:name>%s</domain:name><domain:trStatus>%s</domain:trStatus><domain:reID>%s</domain:reID><domain:reDate>%s</domain:reDate><domain:acID>%s</domain:acID><domain:acDate>%s</domain:acDate></domain:trnData>`, nsDomain, x(name), d.transfer, x(d.transferTo), now.Format(stamp), x(d.clID), now.AddDate(0, 0, 5).Format(stamp)))
	})

	server := &epplib.Server{
		HandleCommand: mux.Handle,
		Greeting:      mux.GetGreeting,
		ConnContext: func(ctx context.Context, _ *tls.Conn) (context.Context, error) {
			return context.WithValue(ctx, sessionKey{}, &session{}), nil
		},
		TLSConfig:      tls.Config{Certificates: []tls.Certificate{pair}, MinVersion: tls.VersionTLS12},
		Timeout:        time.Hour,
		IdleTimeout:    5 * time.Minute,
		WriteTimeout:   time.Minute,
		ReadTimeout:    30 * time.Second,
		MaxMessageSize: 64 * 1024,
		Logger:         slog.New(slog.NewTextHandler(io.Discard, nil)),
	}
	listener, err := net.ListenTCP("tcp", &net.TCPAddr{IP: net.ParseIP("127.0.0.1")})
	if err != nil {
		panic(err)
	}
	ready, _ := json.Marshal(map[string]any{"ready": true, "port": listener.Addr().(*net.TCPAddr).Port})
	fmt.Println(string(ready))
	go func() {
		_, _ = io.Copy(io.Discard, os.Stdin)
		os.Exit(0)
	}()
	if err := server.Serve(listener); err != nil {
		panic(err)
	}
}
