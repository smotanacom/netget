// gofish (an independent Go Redfish client, unchanged) against a Redfish service: session
// login, systems, a reset that returns a task, the task's completion, a PATCH, chassis with
// sensors, managers, sessions, a refused login and logout. Prints one JSON object per step.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"time"

	"github.com/stmcginnis/gofish"
	"github.com/stmcginnis/gofish/schemas"
)

func out(v map[string]any) {
	b, _ := json.Marshal(v)
	fmt.Println(string(b))
}

func fail(step string, err error) {
	out(map[string]any{"step": step, "error": err.Error()})
	os.Exit(1)
}

func main() {
	endpoint, user, password := os.Args[1], os.Args[2], os.Args[3]
	if _, err := gofish.Connect(gofish.ClientConfig{Endpoint: endpoint, Username: user, Password: "wrong", Insecure: true}); err == nil {
		fail("bad_login", fmt.Errorf("a wrong password was accepted"))
	} else {
		out(map[string]any{"step": "bad_login", "refused": true, "error": err.Error()})
	}
	c, err := gofish.Connect(gofish.ClientConfig{Endpoint: endpoint, Username: user, Password: password, Insecure: true})
	if err != nil {
		fail("login", err)
	}
	svc := c.Service
	out(map[string]any{"step": "root", "redfish_version": svc.RedfishVersion, "uuid": svc.UUID, "product": svc.Product})

	systems, err := svc.Systems()
	if err != nil {
		fail("systems", err)
	}
	var list []map[string]any
	for _, s := range systems {
		types, _ := s.GetSupportedResetTypes()
		list = append(list, map[string]any{"id": s.ID, "name": s.Name, "power": s.PowerState, "model": s.Model,
			"cpus": s.ProcessorSummary.Count, "memory_gib": s.MemorySummary.TotalSystemMemoryGiB, "reset_types": types, "asset_tag": s.AssetTag})
	}
	out(map[string]any{"step": "systems", "systems": list})

	sys := systems[0]
	info, err := sys.Reset(schemas.ForceRestartResetType)
	if err != nil {
		fail("reset", err)
	}
	if info == nil || info.Task == nil {
		fail("reset", fmt.Errorf("reset returned no task monitor"))
	}
	out(map[string]any{"step": "reset", "monitor": info.TaskMonitor, "task": info.Task.ODataID, "state": info.Task.TaskState})
	var task *schemas.Task
	for i := 0; i < 50; i++ {
		task, err = schemas.GetTask(c, info.Task.ODataID)
		if err != nil {
			fail("task", err)
		}
		if task.TaskState == schemas.CompletedTaskState {
			break
		}
		time.Sleep(100 * time.Millisecond)
	}
	out(map[string]any{"step": "task", "state": task.TaskState, "percent": task.PercentComplete, "status": task.TaskStatus})

	sys.AssetTag = "netget-asset-1"
	if err := sys.Update(); err != nil {
		fail("patch", err)
	}
	again, err := schemas.GetComputerSystem(c, sys.ODataID)
	if err != nil {
		fail("patch", err)
	}
	out(map[string]any{"step": "patch", "asset_tag": again.AssetTag})

	chassis, err := svc.Chassis()
	if err != nil {
		fail("chassis", err)
	}
	var clist []map[string]any
	for _, ch := range chassis {
		sensors, err := ch.Sensors()
		if err != nil {
			fail("sensors", err)
		}
		var slist []map[string]any
		for _, s := range sensors {
			slist = append(slist, map[string]any{"id": s.ID, "reading": s.Reading, "units": s.ReadingUnits, "type": s.ReadingType})
		}
		clist = append(clist, map[string]any{"id": ch.ID, "type": ch.ChassisType, "sensors": slist})
	}
	out(map[string]any{"step": "chassis", "chassis": clist})

	managers, err := svc.Managers()
	if err != nil {
		fail("managers", err)
	}
	var mlist []map[string]any
	for _, m := range managers {
		mlist = append(mlist, map[string]any{"id": m.ID, "type": m.ManagerType, "firmware": m.FirmwareVersion})
	}
	out(map[string]any{"step": "managers", "managers": mlist})

	ss, err := svc.SessionService()
	if err != nil {
		fail("sessions", err)
	}
	sessions, err := ss.Sessions()
	if err != nil {
		fail("sessions", err)
	}
	out(map[string]any{"step": "sessions", "count": len(sessions), "timeout": ss.SessionTimeout})

	c.Logout()
	out(map[string]any{"step": "logout"})
}
