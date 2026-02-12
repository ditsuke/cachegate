# TODOs

## Performance

- [ ] Stream object instead of fetch -> save -> serve.
- [ ] What happens if client disconnects early? I want cache writes to go through still.

## Inline with Scope

- [ ] More store types (GCS, filesystem, etc).
- [ ] Fixed-token auth, when pre-signing is not necessary.

## Scaling

- [ ] Hybrid in-memory + disk cache. In-memory is faster but more expensive, so we can use it for hot objects and fall back to disk for less popular ones. This will allow us to scale the cache beyond available memory while still on a single node. We can use [foyer](https://github.com/foyer-rs/foyer) as the backbone.
- [ ] Gateway node
    If a single node is unable to handle workload, we'll want to scale it out. At that point to maintain
    cache hits we'll want consistent-ish routing of requests to available nodes.
    **Note**: We might just want to use rendevous hashing to sidestep the [cascading overload
problem](https://arxiv.org/abs/1908.08762), but with the usecase I'm writing this for that's not so relevant.

- [ ] Multiple buckets to serve the same logical bucket. Could be useful to fulfill QoS requirements.

## Scope Expansion

- [ ] Expose PUT and DELETE endpoints
- [ ] Object paging. Cachey has this as a **requirement**. We could have an _optional_ version of this design.
