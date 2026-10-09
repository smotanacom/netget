// Unchanged third-party 9P2000 implementations driven against NetGet:
//
//	peer 9fans ADDR   9fans.net/go's plan9/client runs a fixed scenario, prints one JSON line
//	peer go9p ADDR    knusbaum/go9p's client runs the same scenario, prints one JSON line
//	peer server       knusbaum/go9p's in-memory file server; prints "READY 127.0.0.1:PORT"
//
// The scenario reads, lists, stats, creates, writes, renames and removes, and records each
// error the server gave as text, so the test asserts both success and refusal paths.
package main

import (
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"sort"

	"9fans.net/go/plan9"
	nineclient "9fans.net/go/plan9/client"
	"github.com/knusbaum/go9p"
	go9pclient "github.com/knusbaum/go9p/client"
	"github.com/knusbaum/go9p/fs"
	"github.com/knusbaum/go9p/proto"
)

func errText(err error) string {
	if err == nil {
		return ""
	}
	return err.Error()
}

func run9fans(addr string) map[string]any {
	out := map[string]any{}
	conn, err := nineclient.Dial("tcp", addr)
	if err != nil {
		return map[string]any{"fatal": errText(err)}
	}
	fsys, err := conn.Attach(nil, "glenda", "")
	if err != nil {
		return map[string]any{"fatal": errText(err)}
	}
	if fid, err := fsys.Open("/", plan9.OREAD); err == nil {
		dirs, err := fid.Dirreadall()
		names := []string{}
		for _, d := range dirs {
			names = append(names, d.Name)
		}
		sort.Strings(names)
		out["root"], out["root_error"] = names, errText(err)
		fid.Close()
	} else {
		out["root_error"] = errText(err)
	}
	if fid, err := fsys.Open("/readme.txt", plan9.OREAD); err == nil {
		b, err := io.ReadAll(fid)
		out["readme"], out["readme_error"] = string(b), errText(err)
		fid.Close()
	} else {
		out["readme_error"] = errText(err)
	}
	if fid, err := fsys.Open("/bin.dat", plan9.OREAD); err == nil {
		b, _ := io.ReadAll(fid)
		out["bin"] = hex.EncodeToString(b)
		fid.Close()
	}
	if d, err := fsys.Stat("/docs/guide.md"); err == nil {
		out["stat"] = map[string]any{"name": d.Name, "length": d.Length, "mode": uint32(d.Mode), "uid": d.Uid, "mtime": d.Mtime, "dir": d.Mode&plan9.DMDIR != 0}
	} else {
		out["stat_error"] = errText(err)
	}
	if fid, err := fsys.Open("/many", plan9.OREAD); err == nil {
		dirs, err := fid.Dirreadall()
		out["many"], out["many_error"] = len(dirs), errText(err)
		fid.Close()
	}
	if fid, err := fsys.Create("/scratch/9fans.txt", plan9.OWRITE, 0644); err == nil {
		n, err := fid.Write([]byte("written by 9fans"))
		out["written"], out["write_error"] = n, errText(err)
		fid.Close()
	} else {
		out["create_error"] = errText(err)
	}
	d := plan9.Dir{}
	d.Null()
	d.Name = "renamed.txt"
	out["rename_error"] = errText(fsys.Wstat("/scratch/9fans.txt", &d))
	out["remove_error"] = errText(fsys.Remove("/scratch/renamed.txt"))
	_, err = fsys.Open("/missing.txt", plan9.OREAD)
	out["missing_error"] = errText(err)
	_, err = fsys.Create("/readonly.txt", plan9.OWRITE, 0644)
	out["denied_error"] = errText(err)
	fsys.Close()
	return out
}

func runGo9p(addr string) map[string]any {
	out := map[string]any{}
	nc, err := net.Dial("tcp", addr)
	if err != nil {
		return map[string]any{"fatal": errText(err)}
	}
	c, err := go9pclient.NewClient(nc, "glenda", "")
	if err != nil {
		return map[string]any{"fatal": errText(err)}
	}
	if stats, err := c.Readdir("/"); err == nil {
		names := []string{}
		for _, s := range stats {
			names = append(names, s.Name)
		}
		sort.Strings(names)
		out["root"] = names
	} else {
		out["root_error"] = errText(err)
	}
	if f, err := c.Open("/readme.txt", proto.Oread); err == nil {
		b, err := io.ReadAll(f)
		out["readme"], out["readme_error"] = string(b), errText(err)
		f.Close()
	} else {
		out["readme_error"] = errText(err)
	}
	if s, err := c.Stat("/docs/guide.md"); err == nil {
		out["stat"] = map[string]any{"name": s.Name, "length": s.Length, "mode": s.Mode, "uid": s.Uid, "mtime": s.Mtime, "dir": s.Mode&proto.DMDIR != 0}
	} else {
		out["stat_error"] = errText(err)
	}
	if stats, err := c.Readdir("/many"); err == nil {
		out["many"] = len(stats)
	} else {
		out["many_error"] = errText(err)
	}
	// go9p's Create returns a File with iounit 0, whose writes send zero bytes forever
	// against any server, so the file is created, closed and reopened for writing.
	if f, err := c.Create("/scratch/go9p.txt", 0644); err == nil {
		f.Close()
		if f, err := c.Open("/scratch/go9p.txt", proto.Owrite); err == nil {
			n, err := f.Write([]byte("written by go9p"))
			out["written"], out["write_error"] = n, errText(err)
			f.Close()
		} else {
			out["open_error"] = errText(err)
		}
	} else {
		out["create_error"] = errText(err)
	}
	_, err = c.Open("/missing.txt", proto.Oread)
	out["missing_error"] = errText(err)
	_, err = c.Create("/readonly.txt", 0644)
	out["denied_error"] = errText(err)
	return out
}

func serve() {
	files, root := fs.NewFS("glenda", "glenda", 0777,
		fs.IgnorePermissions(),
		fs.WithRemoveFile(fs.RMFile),
		fs.WithCreateFile(func(f *fs.FS, parent fs.Dir, user, name string, perm uint32, mode uint8) (fs.File, error) {
			file := fs.NewStaticFile(f.NewStat(name, user, user, perm), []byte{})
			return file, parent.(fs.ModDir).AddChild(file)
		}),
		fs.WithCreateDir(func(f *fs.FS, parent fs.Dir, user, name string, perm uint32, mode uint8) (fs.Dir, error) {
			dir := fs.NewStaticDir(f.NewStat(name, user, user, perm))
			return dir, parent.(fs.ModDir).AddChild(dir)
		}),
	)
	root.AddChild(fs.NewStaticFile(files.NewStat("hello.txt", "glenda", "glenda", 0644), []byte("hello from go9p\n")))
	sub := fs.NewStaticDir(files.NewStat("sub", "glenda", "glenda", 0755))
	root.AddChild(sub)
	sub.AddChild(fs.NewStaticFile(files.NewStat("nested.txt", "glenda", "glenda", 0644), []byte("nested file\n")))
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	fmt.Println("READY " + ln.Addr().String())
	srv := files.Server()
	for {
		c, err := ln.Accept()
		if err != nil {
			os.Exit(1)
		}
		go func() {
			defer c.Close()
			go9p.ServeReadWriter(c, c, srv)
		}()
	}
}

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: peer 9fans|go9p ADDR | server")
		os.Exit(2)
	}
	// NINEP_PEER_VERBOSE=1 traces every go9p message on stderr, for debugging.
	go9p.Verbose = os.Getenv("NINEP_PEER_VERBOSE") != ""
	var out map[string]any
	switch os.Args[1] {
	case "9fans":
		out = run9fans(os.Args[2])
	case "go9p":
		out = runGo9p(os.Args[2])
	case "server":
		serve()
		return
	default:
		os.Exit(2)
	}
	b, _ := json.Marshal(out)
	fmt.Println(string(b))
}
