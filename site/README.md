# nubla x — website

The landing page and documentation site for **nubla x**. Plain static files, no build step.

| File | What it is |
|---|---|
| `index.html` | The landing page — hero, feature grid, the Jira ticket board, harnesses, quickstart. |
| `docs.html` | A client-side docs viewer that renders the real Markdown in `docs/`. |
| `docs/*.md` | Copies of the top-level `docs/` guide, served to the viewer. |
| `assets/` | Screenshots (`screenshot.png`, `nebula-board.png`). |
| `favicon.svg` | The tab icon. |
| `vercel.json` | Clean-URL + caching config for Vercel. |

## Run locally

```sh
cd site
python3 -m http.server 8799
# open http://localhost:8799
```

Any static file server works — the docs viewer just needs `docs/*.md` served over HTTP
(a `file://` open won't fetch them).

## Deploy to Vercel

```sh
cd site
npx vercel login
npx vercel deploy --prod --yes
```

Or import `ameerdhi7/NebulaX` in the Vercel dashboard with **Root Directory = `site`**.

## Naming

The product display name is **nubla x**; the binary and terminal command stay `nebula`.
Forked from [AgentSystemLabs/nebula](https://github.com/AgentSystemLabs/nebula).
