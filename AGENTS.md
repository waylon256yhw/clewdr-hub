# AGENTS.md

Instructions for AI agents asked to deploy, run, or develop clewdr-hub.

## Deploying it (end user)

Do not build from source for production. Use a prebuilt release.

**Install script** — fetches the latest prebuilt binary and registers autostart:

```bash
curl -fL https://raw.githubusercontent.com/waylon256yhw/clewdr-hub/master/scripts/install.sh | bash
clewdr menu
```

**Docker** — prebuilt multi-arch image on GHCR:

```bash
docker compose up -d   # docker-compose.yml is in the repo root
# or:
docker run -d --name clewdr-hub -p 8484:8484 \
  -v clewdr-data:/etc/clewdr ghcr.io/waylon256yhw/clewdr-hub:latest
```

Admin console at `http://<host>:8484`. Release builds print a random initial
password on first startup (or preset `ADMIN_PASSWORD`); debug builds use
`password`.

Other methods (BT panel, Hugging Face Space): <https://waylon256yhw.github.io/clewdr-hub/start/installation/>

## Developing it (contributor)

Start from the development docs: <https://waylon256yhw.github.io/clewdr-hub/dev/environment/>

```bash
./scripts/setup-dev.sh   # install/check system + frontend deps
./dev.sh                 # start the dev backend
```

## Docs

Full documentation: <https://waylon256yhw.github.io/clewdr-hub/>
