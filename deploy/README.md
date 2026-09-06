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

Copy [`docker-compose.example.yml`](./docker-compose.example.yml) to
`docker-compose.yaml` in the deploy checkout and edit it there. **That copy is not
tracked by this repo** — the root `/docker-compose.yaml` is in `.gitignore`, precisely
so your live deployment config and git stay out of each other's way:

- `git pull` can't refuse because the live file has local edits, and can't overwrite
  those edits either;
- `git reset --hard` and branch switches leave it alone (untracked files survive both),
  so the checkout can move between `feat/apply-updates` and any other branch safely;
- host-specific paths, hostnames and auth labels never get pushed to the fork.

The flip side: your live compose file now exists **only on the host**, like
`~/docker/cup/cup.json`. Back both up somewhere outside the checkout — a `git clean -fdx`
in that directory would delete them.

Three changes versus a stock Cup deployment:

1. **Mount your compose-projects directory at the same absolute path** it has on the
   host: `-v /home/caradoc/docker-compose:/home/caradoc/docker-compose`. This is where
   your compose files and `.env` files live; the absolute folder paths Cup reads from
   container labels must resolve to the same location inside the container. You do
   **not** need to mount your volume-data directory (e.g. `~/docker`) — bind mounts are
   resolved on the host by the Docker daemon, not inside Cup's container, so keeping it
   out reduces what Cup can touch.
2. **Exclude it from Watchtower** (`com.centurylinklabs.watchtower.enable=false`) so
   your custom image isn't replaced by upstream's `:latest`.
3. **Keep the `build:` section.** The image tag is local — it exists in no registry — so
   `docker compose pull` fetches nothing and a plain `up -d` restarts the *stale* image.
   With `build:` present, `docker compose up -d --build` after a `git pull` is all it
   takes to get the new binary running.

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

## Staying current with upstream Cup

This fork is **upstream + our commits on top**, with two remotes:

- `upstream` → `sergi0g/cup` (the original)
- `origin` → your fork (e.g. `caracturus/cup`)

Our work lives on the `feat/apply-updates` branch. "Updating" means replaying our
commits on top of the newer upstream release.

> If your deploy clone only has `origin`, add upstream first:
> `git remote add upstream https://github.com/sergi0g/cup.git`

### How you'll know there's an update

Watch the [releases page](https://github.com/sergi0g/cup/releases) (GitHub → *Watch →
Custom → Releases*), or just run `git fetch upstream --tags` periodically — new release
tags will appear.

### Update procedure (rebase — recommended)

Do this in **one** clone (the one with both remotes), then sync the other:

```bash
git fetch upstream --tags                  # get new releases
git checkout feat/apply-updates
git rebase v3.6.0                          # ← use the NEW release tag, not main
# resolve conflicts if prompted (see below), then:
git push --force-with-lease origin feat/apply-updates
```

Then rebuild + redeploy on the host:

```bash
git fetch origin && git reset --hard origin/feat/apply-updates   # sync this clone
docker compose up -d --build                                     # rebuild + recreate
```

`--build` is not optional: without it compose reuses the existing
`cup-apply-updates:latest` and the container comes back on the old binary. Confirm the
image is newer than the commit you just checked out with:

```bash
docker image inspect --format '{{.Created}}' cup-apply-updates:latest
```

The `reset --hard` above is safe for your `docker-compose.yaml`: it is untracked, and
`reset --hard` does not touch untracked files.

Rebase onto the release **tag** (`v3.6.x`), not `upstream/main` — tags are stable
releases; `main` may contain unreleased work.

### Handling conflicts

A conflict only happens if upstream changed the same lines we did (most likely in
`src/server.rs`, `src/docker.rs`, or the touched web components). Git pauses and marks
the spots with `<<<<<<<` / `=======` / `>>>>>>>`. To resolve:

1. Edit each marked file to keep the right combination of both changes.
2. `git add <file>`
3. `git rebase --continue`

If it gets messy, **`git rebase --abort`** returns you safely to where you started.

### Simpler alternative (merge — no force-push)

If force-pushing feels risky, merge instead — slightly messier history, but it never
rewrites it:

```bash
git fetch upstream --tags
git checkout feat/apply-updates
git merge v3.6.0          # resolve conflicts the same way, then it commits
git push origin feat/apply-updates   # no --force needed
```

### Notes

- Do the update in **one** clone, push to `origin`, then `git fetch && git reset --hard
  origin/feat/apply-updates` on the other clone. Don't rebase the same branch in two
  places independently.
- Most updates should apply cleanly — our changes are mostly additive (new files) plus
  small edits.
