local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
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
