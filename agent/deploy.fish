#!/usr/bin/env fish

# Load environment variables from .env file
if test -f ../.env
    for line in (cat ../.env | grep -v '^#' | grep -v '^$')
        set -gx (echo $line | cut -d= -f1) (echo $line | cut -d= -f2-)
    end
end

npx cdk deploy $argv
