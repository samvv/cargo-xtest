# k3d fixtures

A `kind = "k3d"` fixture creates a Kubernetes cluster with
`k3d cluster create` and removes it with `k3d cluster delete`. The host needs
`k3d` and Docker.

```toml
[fixture.k8s]
kind = "k3d"
scope = "run"
agents = 1
args = ["--no-lb"]
ready = { cmd = "kubectl wait --for=condition=Ready nodes --all", timeout = "3m" }
export = { K3D_CLUSTER = "{cluster}" }
runner = { image = "archlinux:latest" }   # needed for `run-in`

[[profile.default.overrides]]
filter = "binary(k8s)"
uses = ["k8s"]
```

## Keys

| Key | Default | Meaning |
|---|---|---|
| `servers` | `1` | number of server (control-plane) nodes |
| `agents` | `0` | number of agent (worker) nodes |
| `image` | k3d's default | k3s image, e.g. `"rancher/k3s:v1.31.4-k3s1"` |
| `args` | `[]` | extra arguments for `k3d cluster create` |
| `timeout` | `"300s"` | how long cluster creation may take |
| `ready` | none | `{ cmd = "...", timeout = "..." }`; see [Ready check](#ready-check). The timeout defaults to 120 s. |
| `export` | `{}` | env vars for tests; values may use [templates](#templates) |
| `runner` | none | `{ image = "..." }` or `{ build = { dockerfile = "...", context = "..." } }`: the container tests run in with `run-in` |
| `network` | none | share a Docker network with other clusters; see [Networking between clusters](#networking-between-clusters) |

Plus `kind` and `scope`, which every fixture has; see
[configuration](../configuration.md#fixtures-fixturename). Clusters are slow to
create, so `scope = "run"` is usually best.

## What tests get

- `KUBECONFIG`, pointing at the cluster's kubeconfig.
- A `kubectl` on `PATH` that matches the cluster's version. It is the static
  k3s binary copied from the node image, which works as kubectl under that
  name. It is cached once per image on the host, so the host doesn't need
  kubectl installed.

## Service or environment

| Used with | Where the test runs |
|---|---|
| `uses = ["k8s"]` | on the host, next to the cluster |
| `run-in = "k8s"` | in the `runner` container: host network, sandbox at its original paths (see [environment containers](docker.md#environment-containers)) |

`run-in` with a k3d fixture that has no `runner` is rejected when the config
is loaded.

## Templates

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

## Ready check

`ready.cmd` runs on the host, with the bundled `kubectl` on `PATH`, until it
exits 0 or `ready.timeout` passes:

```toml
ready = { cmd = "kubectl wait --for=condition=Ready nodes --all --timeout=10s", timeout = "3m" }
```

## Several clusters

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

## Networking between clusters

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

## Names and cleanup

Clusters are named `xt<lease>-<run id>` and labeled with their run. If an
agent dies, the next agent on that host deletes its leftover clusters and
networks when it starts.

Cached kubectl binaries are removed after 7 days without use, or by
`cargo xtest clean`; see [cleanup](../cleanup.md).
