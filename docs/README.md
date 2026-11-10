# cargo-xtest documentation

| Page | Contents |
|---|---|
| [configuration.md](configuration.md) | every key in `.config/xtest.toml`: profiles, overrides, fixtures, hosts |
| [filters.md](filters.md) | the filter syntax for `-E` and overrides |
| [fixtures/](fixtures/README.md) | what all fixtures share: kinds, services and environments, scopes, `--keep-fixtures` |
| [fixtures/docker.md](fixtures/docker.md) | Docker containers as services or environments |
| [fixtures/command.md](fixtures/command.md) | your own setup and teardown commands |
| [fixtures/k3d.md](fixtures/k3d.md) | Kubernetes clusters with k3d, several clusters, and networking between them |
| [fixtures/rust.md](fixtures/rust.md) | fixtures written in Rust with `#[xtest::fixture]`: building blocks, dependencies, teardown |
| [xtest-test.md](xtest-test.md) | the `#[xtest::test]` attribute from the `xtest` crate |
| [memory-scheduling.md](memory-scheduling.md) | how tests start only while they fit in free memory |
| [cleanup.md](cleanup.md) | what agents store and clean up, and `cargo xtest clean` |
| [internals.md](internals.md) | how cargo-xtest works inside, for contributors |
