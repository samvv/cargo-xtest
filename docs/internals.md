# cargo-xtest internals

Details behind the configuration in the [README](../README.md): how tests are
selected, and how environment containers and k3d fixtures work.

## Filters

Filters choose which tests an override applies to (`filter = "..."` in
`.config/xtest.toml`) and which tests run (`-E` on the command line).

### Predicates

| Predicate | Matches | Example |
|---|---|---|
| `package(..)` | the package name | `package(myapp)` |
| `binary(..)` | the test binary (target) name, e.g. `k8s` for `tests/k8s.rs` | `binary(k8s)` |
| `kind(..)` | the target kind: `lib`, `bin`, `test`, `bench`, `example` or `proc-macro` | `kind(test)` |
| `test(..)` | the test's name, e.g. `backup::tests::creates_archive` | `test(archive)` |
| `all()`, `none()` | every test, or none | `all()` |

### Matchers

| Syntax | Meaning |
|---|---|
| `=value` | exactly `value` |
| `~value` | contains `value` |
| `/regex/` | matches the regular expression |
| `value` (no prefix) | contains `value` for `test()`; exactly `value` for the others |

### Operators

From weakest to strongest binding: `|` (or `or`), `&` (or `and`), and `!`
(or `not`). Use parentheses to group.

```
package(db) & test(/integration/) & !kind(bench)
binary(k8s) | binary(k8s_env)
```

Several `-E` flags are combined with *or*: a test runs if any of them matches.

## Environment containers

A Docker fixture used with `run-in` is the test's *environment*: the test
process runs inside the container (through `docker exec`). The container:

- runs as your uid and gid, with `USER` and `LOGNAME` set;
- uses the host network (`--network host`);
- has the run's sandbox mounted at its original absolute paths, so the test
  sees the same paths as on your machine;
- starts with `sleep infinity` as entrypoint, so it stays up until teardown.

The test binary is built on your machine and runs unchanged in the image. The
image therefore needs a glibc at least as new as yours, unless you build for a
musl target (`target = "x86_64-unknown-linux-musl"`).

## k3d fixtures

A `kind = "k3d"` fixture creates a Kubernetes cluster with
`k3d cluster create` and removes it with `k3d cluster delete`. The host needs
`k3d` and Docker.

### What tests get

- `KUBECONFIG`, pointing at the cluster's kubeconfig.
- A `kubectl` on `PATH` that matches the cluster's version. It is the static
  k3s binary copied from the node image, which works as kubectl under that
  name. It is cached once per image on the host, so the host doesn't need
  kubectl installed.

### Service or environment

| Used with | Where the test runs |
|---|---|
| `uses = ["k8s"]` | on the host, next to the cluster |
| `run-in = "k8s"` | in a `runner` container: host network, sandbox at its original paths |

`run-in` requires a runner image:
`runner = { image = "..." }` or `runner = { build = { dockerfile = "...", context = "..." } }`.

### Templates

`export` and `ready.cmd` can use these templates:

| Template | Value |
|---|---|
| `{cluster}` | the cluster's name |
| `{context}` | its kubeconfig context, `k3d-<cluster>` |
| `{kubeconfig}` | path to its kubeconfig |
| `{network}` | the Docker network its nodes are on |
| `{server-ip}` | IP of the first server node |
| `{server:N}`, `{agent:N}` | IP of server or agent node `N` |

A node template for a node that doesn't exist, such as `{agent:3}` on a
cluster with one agent, is an error. Other braces, such as shell `${X}` or
literal JSON, are left alone.

### Ready check

`ready.cmd` runs on the host, with the bundled `kubectl` on `PATH`, until it
exits 0 or `ready.timeout` passes:

```toml
ready = { cmd = "kubectl wait --for=condition=Ready nodes --all --timeout=10s", timeout = "3m" }
```

### Several clusters

A test that uses several clusters gets all their kubeconfigs in `KUBECONFIG`
(`a.yaml:b.yaml`). kubectl reads all of them; pick a cluster with
`--context`. Exporting each cluster's context makes that easy:

```toml
[fixture.mesh-a]
kind = "k3d"
export = { A_CONTEXT = "{context}" }
```

```
kubectl --context "$A_CONTEXT" get pods
```

### Networking between clusters

Each cluster gets its own Docker network by default, and Docker blocks
traffic between networks: a pod in one cluster can't reach another
cluster's nodes.

Clusters that set the same `network = "name"` share one network instead.
Their nodes can then reach each other, for example through a NodePort:

```toml
[fixture.mesh-a]
kind = "k3d"
network = "mesh"
export = { A_CONTEXT = "{context}", A_SERVER_IP = "{server-ip}" }

[fixture.mesh-b]
kind = "k3d"
network = "mesh"
export = { B_CONTEXT = "{context}" }
```

A pod in `mesh-b` can now reach `http://$A_SERVER_IP:<NodePort>/` of a
service in `mesh-a`. The agent creates the network for the run
(`xt-<run id>-<name>`) and removes it at the end of the run.

### Names and cleanup

Clusters are named `xt<lease>-<run id>` and labeled with their run. If an
agent dies, the next agent on that host deletes its leftover clusters and
networks when it starts.
