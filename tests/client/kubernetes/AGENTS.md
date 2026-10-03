# Kubernetes Client Tests

## Strategy

**No test here talks to a real Kubernetes cluster, and none pretends to.**

This file used to be a page of minikube/kind setup instructions describing three "integration
tests" that were `#[ignore]`d, gated on `kubectl cluster-info`, and asserted **nothing** — they
printed `Full E2E test implementation requires NetGet binary integration` and returned. A test
that is green whether the client works or not is not evidence; they have been deleted, and the
client's `metadata().e2e_testing` says as much. The client stays `Experimental` for that reason.

What the suite actually covers is the wire path, without a cluster and without a model:

| File | LLM calls | What it proves |
|---|---|---|
| `command_channel_test.rs` | 0 | `AppState::send_to_client` injects an action into a running client, the command loop runs it through the same `apply_action` every path uses, and the request **really reaches a listener** — `stub.saw("/api/v1/namespaces/default/pods")`. Also: an unknown verb comes back `Rejected`, `disconnect` comes back `Disconnected`, and the command handle is gone afterwards |
| `e2e_test.rs` | 0 | registry identity; every advertised verb executes into a `k8s_operation` carrying the right `operation`/`resource_type`; an unknown verb is refused; `get_event_types()` returns the statics that actually fire, with their parameters |

`command_channel_test.rs` points the client's LLM at `http://127.0.0.1:1`, so the
`k8s_resource_received` call *fails* — tolerating that is part of what the test verifies.

## Privacy

`pin_kubeconfig_to_loopback` writes a throwaway kubeconfig whose single cluster is a loopback
TCP listener and sets `KUBECONFIG` to it. Without that, `kube::Client::try_default()` reads the
developer's real `~/.kube/config` and the test talks to their actual cluster. Anything added
here must keep that property.

It also clears `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` (and the lowercase forms): `kube` refuses
to build a client at all when a proxy is configured unless the optional `kube/http-proxy`
feature is on, which NetGet does not enable. Everything under test is on loopback, which no
proxy should handle.

## rustls provider

`install_rustls_provider()` installs `ring` through `quinn`'s rustls re-export. This is now
belt-and-braces rather than a workaround: `connect_with_llm_actions` installs the provider
itself, and `install_default` returning `Err` because one is already set is the wanted outcome.
See `src/client/kubernetes/CLAUDE.md` for why two providers can be linked at once.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features kubernetes \
    --test client -- --test-threads=100 kubernetes
```

Note `--test client`, not `--test kubernetes`: these compile into `tests/client.rs` through
`tests/client/mod.rs`. Runtime: under 5 seconds.

## What would make this client Beta

A test in which a **real** Kubernetes apiserver (kind or minikube) answers, driven from CI or at
least hard-failing when absent the way `tests/server/kubernetes/e2e_test.rs` does for `kubectl`.
A skip-when-missing gate would put this straight back where it started.
