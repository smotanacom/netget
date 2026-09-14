#!/bin/bash
# Publish netget.net — the static landing page in this directory — to S3 + CloudFront.
#
#   S3 bucket:    netget.net (us-east-1, private; CloudFront reads it through an OAC)
#   Distribution: E2LPP647KD7BN2  (netget.net, *.netget.net)
#   Certificate:  ACM us-east-1, DNS-validated
#   DNS:          Porkbun — apex ALIAS + www CNAME to the CloudFront domain
#
# The files here are the site, plus demo/pkg/, which ./web/build.sh writes (the
# NetGet-in-the-browser bundle; gitignored) and which must exist before this runs.
# Everything but index.html is cached for a week, index.html is not cached, and
# the whole distribution is invalidated afterwards.

set -ex

export AWS_PROFILE=smotana

BUCKET=netget.net
DISTRIBUTION_ID=E2LPP647KD7BN2

cd "$(dirname "$0")"

if [ ! -f demo/pkg/netget_web_bg.wasm ]; then
  echo "demo/pkg/netget_web_bg.wasm is missing: run ./web/build.sh first" >&2
  exit 1
fi

aws s3 sync . "s3://$BUCKET/" --delete \
  --exclude 'deploy.sh' --exclude '*.md' --exclude '.DS_Store' --exclude 'index.html' \
  --cache-control "max-age=604800"

aws s3 sync . "s3://$BUCKET/" \
  --exclude '*' --include 'index.html' \
  --cache-control "max-age=0"

# The AWS CLI guesses content types from the extension and does not know .wasm;
# without application/wasm the browser falls back to a slower, non-streaming compile.
aws s3 cp demo/pkg/netget_web_bg.wasm "s3://$BUCKET/demo/pkg/netget_web_bg.wasm" \
  --content-type application/wasm --cache-control "max-age=604800" \
  --metadata-directive REPLACE

aws cloudfront create-invalidation --distribution-id "$DISTRIBUTION_ID" --paths "/*"
