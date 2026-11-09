#!/bin/sh
# The suite-wide counterpart of `isolated_home` in maki-providers/src/test_support.rs:
# keep the directories and the base URL suffix in step with it.
set -eu

home=$(mktemp -d "${TMPDIR:-/tmp}/maki-test-home.XXXXXX")
{
    echo "HOME=$home"
    echo "XDG_CONFIG_HOME=$home/config"
    echo "XDG_STATE_HOME=$home/state"
    echo "XDG_DATA_HOME=$home/data"
    echo "XDG_CACHE_HOME=$home/cache"
    env | sed -n 's/^\([A-Za-z0-9_]*_BASE_URL\)=.*/\1=/p'
} >>"$NEXTEST_ENV"
