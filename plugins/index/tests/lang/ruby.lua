local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local idx_with_meta = helpers.idx_with_meta
local has = helpers.has

case("ruby_all_sections", function()
  local src = [==[
require "net/http"
require_relative "lib/helper"

MAX_RETRIES = 3
TIMEOUT = 30

module Utilities
  class Parser
    def parse(input)
    end
  end
end

class Animal
  def initialize(name)
  end
  def speak
  end
end

class Dog < Animal
  def initialize(name, breed)
  end
  def self.create(name)
  end
  def fetch(item)
  end
end

def standalone(x, y)
end

def self.class_fn(opts = {})
end
]==]
  local out = idx(src, "ruby")
  has(out, {
    "imports:",
    "net/http",
    "lib/helper",
    "consts:",
    "MAX_RETRIES = 3",
    "TIMEOUT = 30",
    "mod:",
    "Utilities",
    "classes:",
    "Parser",
    "parse(input)",
    "Animal",
    "initialize(name)",
    "speak()",
    "Dog < Animal",
    "initialize(name, breed)",
    "self.create(name)",
    "fetch(item)",
    "fns:",
    "standalone(x, y)",
  })
end)

case("ruby_class_methods_have_ranged_meta", function()
  local src = [==[
class Greeter
  def hello(name)
    puts name
  end
  def goodbye(name)
    puts name
  end
end
]==]
  local text, meta = idx_with_meta(src, "ruby")
  helpers.assert_ranged_meta(text, meta, { "hello", "goodbye" })
end)
