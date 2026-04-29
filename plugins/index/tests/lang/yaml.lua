local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

case("yaml_top_level_keys", function()
  local src = [==[
name: maki
version: "0.4.0"
description: AI coding agent
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "name",
    "version",
    "description",
  })
end)

case("yaml_nested_mapping_as_children", function()
  local src = [==[
services:
  web:
    image: nginx
    port: 80
  db:
    image: postgres
    port: 5432
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "services",
    "web",
    "db",
  })
  lacks(out, {
    "image",
    "port",
  })
end)

case("yaml_sequence_values_not_indexed", function()
  local src = [==[
env:
  - FOO=1
  - BAR=2
ports:
  - 8080
  - 8443
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "env",
    "ports",
  })
  lacks(out, {
    "FOO=1",
    "BAR=2",
    "8080",
  })
end)

case("yaml_flow_mapping_keys", function()
  local src = [==[
meta: {a: 1, b: 2}
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "meta",
    "a",
    "b",
  })
end)

case("yaml_quoted_keys_preserved", function()
  local src = [==[
"full name": maki
'machine': x86
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "full name",
    "machine",
  })
end)

case("yaml_multi_document_stream", function()
  local src = [==[
---
title: first
---
title: second
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "title",
  })
end)

case("yaml_sequence_of_mappings", function()
  local src = [==[
items:
  - name: first
    value: 1
  - name: second
    value: 2
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "items",
    "name",
    "value",
  })
  lacks(out, {
    "first",
    "second",
    " 1 ",
    " 2 ",
  })
end)

case("yaml_top_level_sequence_of_mappings", function()
  local src = [==[
- name: alpha
  value: 1
- name: beta
  value: 2
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "name",
    "value",
  })
  lacks(out, {
    "alpha",
    "beta",
  })
end)

case("yaml_flow_sequence_of_inline_tables", function()
  local src = [==[
items: [{name: a}, {name: b}]
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "items",
    "name",
  })
  lacks(out, {
    " a ",
    " b ",
  })
end)

case("yaml_block_scalar_not_indexed", function()
  local src = [==[
key: |
  multi
  line
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "key",
  })
  lacks(out, {
    "multi",
    "line",
  })
end)

case("yaml_null_and_boolean_values", function()
  local src = [==[
enabled: true
count: 42
empty: null
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "enabled",
    "count",
    "empty",
  })
end)

case("yaml_empty_mapping_and_sequence", function()
  local src = [==[
config: {}
items: []
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "config",
    "items",
  })
end)

case("yaml_scalar_only_document", function()
  local out = idx("just a scalar value\n", "yaml")
  lacks(out, {
    "consts:",
    "scalar",
  })
end)

case("yaml_comments_only", function()
  local src = [==[
# just a comment
]==]
  local out = idx(src, "yaml")
  assert(out == "", "expected empty output for comment-only document")
end)

case("yaml_explicit_document_marker", function()
  local src = [==[
%YAML 1.2
---
name: marked
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "name",
  })
end)

case("yaml_nested_flow_mapping_in_block", function()
  local src = [==[
settings:
  tags: [a, b]
  meta: {x: 1}
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "settings",
    "tags",
    "meta",
  })
  lacks(out, {
    " a ",
    " b ",
    " 1 ",
    " x ",
  })
end)

case("yaml_mixed_quoted_and_unquoted_keys", function()
  local src = [==[
"first key": one
second_key: two
'3rd-key': three
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "first key",
    "second_key",
    "3rd-key",
  })
end)

case("yaml_real_world_deployment", function()
  local src = [==[
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 3
  template:
    spec:
      containers:
        - name: nginx
          image: nginx:latest
          ports:
            - containerPort: 80
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "apiVersion",
    "kind",
    "metadata",
    "name",
    "spec",
    "replicas",
    "template",
  })
  lacks(out, {
    "containers",
    "image",
    "ports",
    "nginx",
    "nginx:latest",
    "containerPort",
    "80",
  })
end)

case("yaml_ranged_meta", function()
  local src = [==[
name: maki
metadata:
  author: alice
]==]
  local out, meta = helpers.idx_with_meta(src, "yaml")
  helpers.assert_ranged_meta(out, meta, {
    "name",
    "metadata",
    "author",
  })
end)

case("yaml_github_actions", function()
  local src = [==[
name: CI
on:
  push:
    branches: [main]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo test
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "name",
    "on",
    "push",
    "jobs",
    "build",
  })
  lacks(out, {
    "ubuntu-latest",
    "actions/checkout@v4",
    "cargo test",
    "runs-on",
    "steps",
    "branches",
    "main",
  })
end)

case("yaml_docker_compose", function()
  local src = [==[
version: "3.8"
services:
  web:
    image: nginx:latest
    ports:
      - "8080:80"
    environment:
      - DEBUG=1
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "version",
    "services",
    "web",
  })
  lacks(out, {
    "nginx",
    "8080:80",
    "DEBUG=1",
    "image",
    "ports",
    "environment",
  })
end)

case("yaml_helm_values", function()
  local src = [==[
image:
  repository: nginx
  tag: "1.27"
  pullPolicy: IfNotPresent
service:
  type: ClusterIP
  port: 80
ingress:
  enabled: false
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "image",
    "repository",
    "tag",
    "pullPolicy",
    "service",
    "type",
    "port",
    "ingress",
    "enabled",
  })
  lacks(out, {
    "nginx",
    "1.27",
    "ClusterIP",
    "IfNotPresent",
    " 80 ",
  })
end)

case("yaml_top_level_flow_sequence_of_tables", function()
  local src = [==[
[{name: first}, {name: second}]
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "name",
  })
  lacks(out, {
    " first",
    " second",
  })
end)

case("yaml_trailing_comma_flow_sequence", function()
  local src = [==[
items: [{name: a}, {name: b},]
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "items",
    "name",
  })
end)

case("yaml_empty_and_comment_documents", function()
  local src = [==[
---
# document one
title: first
---
# empty doc
---
title: last
]==]
  local out = idx(src, "yaml")
  has(out, {
    "consts:",
    "title",
  })
end)
