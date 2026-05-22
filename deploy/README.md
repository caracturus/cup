# Cup — "apply updates" fork: deployment guide

This fork adds a button to Cup's web UI: tick the images that have an update and
click **Update**, and Cup runs `docker compose pull && docker compose up -d` in
each one's project folder.

> **Scope:** only **floating-tag (digest) updates** are applied — images using a
> moving tag like `:latest`, where pulling fetches a newer digest. **Version-pinned**
> rows (e.g. `:v1.10.0`) show a ✎ hint instead of a checkbox, because applying those
> requires editing the tag in your compose file by hand first.

## How it works

1. Cup reads each running container's `com.docker.compose.*` labels to learn its
   project folder (`working_dir`).
2. The UI shows a checkbox only on rows that have a digest update **and** are
   compose-managed.
3. `POST /actions/update` resolves the folders **server-side** from those labels
   (never from the request) and runs compose there, via argument arrays (no shell).

## Build the image

The feature isn't in upstream Cup, so build your own image on the host:

```bash
git clone <your-fork-url> cup && cd cup
git checkout feat/apply-updates
docker build -t cup-apply-updates:latest .
```

(The production `Dockerfile` is `alpine` + the docker CLI + compose plugin — it can't
be `scratch` anymore, because applying updates shells out to `docker compose`.)

## Deploy

Use [`docker-compose.example.yml`](./docker-compose.example.yml) as a starting point.
Two changes versus a stock Cup deployment:

1. **Mount your compose-projects directory at the same absolute path** it has on the
   host (e.g. `-v /home/caradoc/docker-compose:/home/caradoc/docker-compose`). The
   folder paths Cup gets from container labels are absolute host paths, so they must
   resolve to the same location inside the container.
2. **Exclude it from Watchtower** (`com.centurylinklabs.watchtower.enable=false`) so
   your custom image isn't replaced by upstream's `:latest`.

## Security checklist (read before exposing)

- **The update endpoint must stay behind authentication.** It lives at
  `/actions/update`, deliberately *outside* `/api/`. In the example, the Authentik
  router matches the whole host (so it covers `/actions/update`), while the
  unauthenticated router is scoped to `/api/` only. **Never add `/actions/` to the
  no-auth router** — that would let anyone recreate your containers.
- **CSRF:** the endpoint has no CSRF token of its own; it relies on the auth proxy in
  front. Authentik's session cookies are `SameSite=Lax`, which blocks cross-site
  state-changing requests. Keep the endpoint behind Authentik and this is covered.
- **Docker socket = root on the host.** Cup already needed the socket to detect
  updates; applying them uses the same access. The auth gate above is the real
  control. The container only mounts the socket and your compose directory.
- **Self-update is refused.** Cup won't recreate its own container (it would kill the
  process mid-request). This matches the running container by ID via `$HOSTNAME`, so
  don't override the container's hostname.
- **Audit trail:** every apply-updates request and per-project result is logged to
  Cup's container logs (`docker logs cup`).

## Rolling back / staying current with upstream

Work lives on the `feat/apply-updates` branch; `main` tracks upstream `sergi0g/cup`.
To pick up a new Cup release: `git fetch upstream && git rebase upstream/main` on the
feature branch, then rebuild.
