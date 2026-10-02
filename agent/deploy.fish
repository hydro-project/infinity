#!/usr/bin/env fish

# Load environment variables from a .env file (this directory or the repo root)
set -l env_file .env
if not test -f $env_file
    set env_file ../.env
end
if test -f $env_file
    for line in (cat $env_file | grep -v '^#' | grep -v '^$')
        set -gx (echo $line | cut -d= -f1) (echo $line | cut -d= -f2-)
    end
end

npx cdk deploy $argv
