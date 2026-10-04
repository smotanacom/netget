// Command fc drives FreeCONF's RESTCONF library (unchanged) for NetGet's tests:
//
//	fc serve PORT   the FreeCONF car example served over RESTCONF on 127.0.0.1:PORT
//	fc client URL   a FreeCONF RESTCONF client against URL (e.g. http://127.0.0.1:1234/restconf),
//	                printing one JSON line per step
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"strings"

	"github.com/freeconf/restconf"
	"github.com/freeconf/restconf/client"
	"github.com/freeconf/restconf/device"
	"github.com/freeconf/yang/node"
	"github.com/freeconf/yang/nodeutil"
	"github.com/freeconf/yang/source"
	"github.com/freeconf/yang/testdata/car"
)

func ypath() source.Opener {
	return source.Any(restconf.InternalYPath, car.YPath)
}

func out(step string, v map[string]interface{}) {
	v["step"] = step
	b, _ := json.Marshal(v)
	fmt.Println(string(b))
}

func asJSON(sel *node.Selection) interface{} {
	if sel == nil {
		return nil
	}
	s, err := nodeutil.WriteJSON(sel)
	if err != nil {
		return map[string]interface{}{"error": err.Error()}
	}
	var v interface{}
	_ = json.Unmarshal([]byte(s), &v)
	return v
}

func errText(err error) interface{} {
	if err == nil {
		return nil
	}
	return err.Error()
}

func serve(port string) {
	c := car.New()
	d := device.New(ypath())
	if err := d.Add("car", car.Manage(c)); err != nil {
		panic(err)
	}
	restconf.NewServer(d)
	cfg := fmt.Sprintf(`{"car":{},"fc-restconf":{"web":{"port":"127.0.0.1:%s"}}}`, port)
	if err := d.ApplyStartupConfig(strings.NewReader(cfg)); err != nil {
		panic(err)
	}
	fmt.Println("restconf listening on", port)
	select {}
}

func runClient(url string) {
	d, err := client.ProtocolHandler(ypath())(url)
	if err != nil {
		out("connect", map[string]interface{}{"error": err.Error()})
		os.Exit(1)
	}
	out("connect", map[string]interface{}{"modules": len(d.Modules())})
	b, err := d.Browser("car")
	if err != nil {
		out("browser", map[string]interface{}{"error": err.Error()})
		os.Exit(1)
	}
	root := b.Root()
	out("read", map[string]interface{}{"data": asJSON(root)})
	tire, err := root.Find("tire=1")
	out("read_tire", map[string]interface{}{"data": asJSON(tire), "error": errText(err)})
	missing, err := root.Find("tire=9")
	out("read_missing", map[string]interface{}{"found": missing != nil, "error": errText(err)})
	edit, _ := nodeutil.ReadJSON(`{"speed":25}`)
	out("edit", map[string]interface{}{"error": errText(root.UpsertFrom(edit))})
	out("read_after", map[string]interface{}{"data": asJSON(root)})
	rpc, err := root.Find("addOil")
	if err != nil || rpc == nil {
		out("rpc", map[string]interface{}{"error": fmt.Sprint("no addOil: ", err)})
		return
	}
	input, _ := nodeutil.ReadJSON(`{"drainFirst":true,"amount":2.5}`)
	result, err := rpc.Action(input)
	out("rpc", map[string]interface{}{"data": asJSON(result), "error": errText(err)})
}

func main() {
	if len(os.Args) != 3 {
		fmt.Fprintln(os.Stderr, "usage: fc serve PORT | fc client URL")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "serve":
		serve(os.Args[2])
	case "client":
		runClient(os.Args[2])
	}
}
