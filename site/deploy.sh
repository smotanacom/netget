#!/bin/bash
# Publish netget.net — the static landing page in this directory — to S3 + CloudFront.
#
#   S3 bucket:    netget.net (us-east-1, private; CloudFront reads it through an OAC)
#   Distribution: E2LPP647KD7BN2  (netget.net, *.netget.net)
#   Certificate:  ACM us-east-1, DNS-validated
#   DNS:          Porkbun — apex ALIAS + www CNAME to the CloudFront domain
#
# Nothing is built: the files here are the site. Everything but index.html is
# cached for a week, index.html is not cached, and the whole distribution is
# invalidated afterwards.

set -ex

export AWS_PROFILE=smotana

BUCKET=netget.net
DISTRIBUTION_ID=E2LPP647KD7BN2

cd "$(dirname "$0")"

aws s3 sync . "s3://$BUCKET/" --delete \
  --exclude 'deploy.sh' --exclude '*.md' --exclude '.DS_Store' --exclude 'index.html' \
  --cache-control "max-age=604800"

aws s3 sync . "s3://$BUCKET/" \
  --exclude '*' --include 'index.html' \
  --cache-control "max-age=0"

aws cloudfront create-invalidation --distribution-id "$DISTRIBUTION_ID" --paths "/*"
