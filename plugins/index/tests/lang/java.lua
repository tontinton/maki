local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has

case("java_all_sections", function()
  local src = [==[
package com.example;

import java.util.List;
import java.io.IOException;

public class Service extends BaseService implements Runnable, Serializable {
    private String name;
    public Service(String name) { this.name = name; }
    @Override
    public String toString() { return name; }
    public void process(List<String> items) throws IOException {}
}

/** Handler docs */
public interface Handler extends Comparable<Handler> {
    void handle(String request);
}

public enum Direction implements Displayable {
    UP, DOWN, LEFT, RIGHT
}
]==]
  local out = idx(src, "java")
  has(out, {
    "imports:",
    "java.{io.IOException, util.List}",
    "mod:",
    "com.example",
    "classes:",
    "public class Service extends BaseService implements Runnable, Serializable",
    "private String name",
    "public Service(String name)",
    "@Override public String toString()",
    "public void process(List<String> items)",
    "traits:",
    "public interface Handler extends Comparable<Handler>",
    "void handle(String request)",
    "types:",
    "public enum Direction implements Displayable",
    "UP, DOWN",
  })
end)
