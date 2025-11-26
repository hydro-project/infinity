#!/usr/bin/env fish

# Load environment variables from .env file
if test -f ../.env
    for line in (cat ../.env | grep -v '^#' | grep -v '^$')
        set -gx (echo $line | cut -d= -f1) (echo $line | cut -d= -f2-)
    end
end

# Deploy the CDK stack with --method=direct to bypass changeset validation
npx cdk deploy --method=direct $argv
