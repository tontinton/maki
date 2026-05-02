local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has

case("scala_all_sections", function()
  local src = [==[
package com.example

import scala.collection.mutable.Map
import java.io.File

val MaxRetries = 3

class Service(name: String) extends Base with Logging {
  def process(input: String): Boolean = true
  def shutdown(): Unit = {}
}

object Config {
  def load(path: String): Config = ???
}

trait Handler {
  def handle(req: Request): Response
}

def helper(x: Int): String = x.toString

type Callback = String => Unit
]==]
  local out = idx(src, "scala")
  has(out, {
    "imports:",
    "scala.collection.mutable.Map",
    "java.io.File",
    "mod:",
    "com.example",
    "classes:",
    "Service",
    "process",
    "shutdown",
    "Config",
    "load",
    "traits:",
    "Handler",
    "fns:",
    "helper",
    "consts:",
    "val MaxRetries",
    "types:",
    "type Callback",
  })
end)
