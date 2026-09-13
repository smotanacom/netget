# netget.net — the landing page

Four files, no build step: `index.html`, `css/style.css`, `js/main.js`, `favicon.svg`.
`./deploy.sh` is the whole publishing pipeline.

What is in this directory is public, with one exception: `deploy.sh` and every `*.md` —
this file included — are excluded from the sync, because this one names infrastructure IDs.
The internal planning documents under `docs/` are not part of the site at all. Adding a
non-markdown file here publishes it, so check before you do.

## Hosting

S3 + CloudFront in AWS account `750984813907` (`AWS_PROFILE=smotana`), the same shape as
the maintainer's other static sites (`zisk.ca`, `ollisten.com`, `matusnomin.com`):

| Piece | Value |
|---|---|
| S3 bucket | `netget.net`, us-east-1, **private** — public access fully blocked, no website config |
| Distribution | `E2LPP647KD7BN2` → `d3m64zfgzk3tzs.cloudfront.net` |
| Aliases | `netget.net`, `*.netget.net` |
| Origin access | OAC `E2IVTMAHEQBL57` (sigv4, always); the bucket policy allows only `cloudfront.amazonaws.com` for this distribution |
| Certificate | ACM us-east-1 `e514d390-7946-4243-8a4d-4e3374aa68de`, DNS-validated, auto-renews |
| Cache policy | Managed-CachingOptimized (`658327ea-…`), compression on |
| Errors | 403 → `/index.html` with status 200 (S3 returns 403, not 404, for a missing key when the OAC has no `ListBucket`) |

The bucket is reachable **only** through CloudFront. A direct
`https://netget.net.s3.amazonaws.com/index.html` returns 403, and that is correct.

## DNS

At **Porkbun** (the registrar is also the nameserver — there is no Route 53 zone for this
domain, and there should not be one):

| Record | Type | Value |
|---|---|---|
| `netget.net` | ALIAS | `d3m64zfgzk3tzs.cloudfront.net` |
| `www.netget.net` | CNAME | `d3m64zfgzk3tzs.cloudfront.net` |
| `_fd521527ba3e64edae0b9e8c783a9628` | CNAME | ACM validation — **do not delete**, the certificate stops renewing without it |

Porkbun's ALIAS flattens at the edge, which is what lets the apex point at CloudFront.

To change DNS from a script: `source ~/bin/porkbun-env`, then POST to
`https://api.porkbun.com/api/json/v3/dns/…` with `apikey` + `secretapikey` in the JSON body.
The apex record's `name` is the empty string, not `@`. The domain must have API access
enabled in the Porkbun panel (Domain Management → netget.net → Details → API ACCESS);
netget.net was not opted in until September 2026.

## What this replaced

GitHub Pages served `docs/` at netget.net via `docs/CNAME`. That stopped being an option
when the repository went private — Pages on a private repository needs a paid plan, and the
site's own links to `github.com/smotanacom/netget` are public-facing links into a private
repository regardless of where the page is hosted.

## Deploying

```bash
./deploy.sh
```

Assets go up with `max-age=604800`, `index.html` with `max-age=0`, then the distribution is
invalidated. Because `index.html` is never cached by browsers and the invalidation clears the
edges, a deploy is visible within a few seconds. The first pass syncs with `--delete`, so a
file removed from this directory is removed from the bucket — except an excluded one, which
`--delete` also skips. Deleting `deploy.sh` or a `*.md` from the bucket is a manual
`aws s3 rm`.
