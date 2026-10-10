# TR-069 ACS tests

`install_peers.py <dir>` installs genieacs-sim 0.9.0 (npm) with libxmljs 1.0.11, GenieACS
1.2.16 (npm) and MongoDB 8.0.4 (SHA-256 checked), and prints `NETGET_GENIEACS_SIM`,
`NETGET_GENIEACS` and `NETGET_MONGOD`. The tests fail without them.

`peer/run-sim.cjs` runs one simulated device in one process (the simulator's CLI forks a
cluster whose workers outlive a killed parent). genieacs-sim was written against libxmljs
0.18, which does not build on current Node; the runner adapts two call shapes for 1.0 (string
coercion, and the one-argument `attr(name)` getter). The simulator's protocol code is
unchanged.

- `genieacs_sim_is_managed_by_netget`: the simulator (a TR-098 Huawei gateway of 1000
  parameters) informs; a python policy queues GetParameterValues, SetParameterValues, a
  read-back (the simulator stored 3600 as `xsd:unsignedInt`), GetParameterNames, AddObject —
  whose response queues a DeleteObject of the new instance — and Reboot, which the simulator
  refuses with fault 9000. Mutation-checked: dropping the model's actions fails it.
- `sessions_faults_and_bounds`: raw HTTP — a refused device (8001), no session (8003), a
  DOCTYPE and nesting past `MAX_DEPTH` (8003), an envelope over `MAX_ENVELOPE` (413), a
  session's cookie and queued RPC, a device fault moving the session on, and no model (8002
  with a category, no cookie).

No LLM calls.
