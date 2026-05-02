local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has

case("python_all_sections", function()
  local src = [==["""Module docstring."""

import os
from typing import Optional

MAX_RETRIES = 3
MY_VAR: int = 10

@dataclass
class MyClass:
    x: int = 0

class AuthService:
    def __init__(self, secret: str):
        self.secret = secret
    @staticmethod
    def validate(token: str) -> bool:
        return True

def process(data: list) -> dict:
    return {}
]==]
  local out = idx(src, "python")
  has(out, {
    "module doc:",
    "imports:",
    "os",
    "typing.Optional",
    "consts:",
    "MAX_RETRIES",
    "MY_VAR = 10",
    "classes:",
    "MyClass [9-11]",
    "@staticmethod",
    "AuthService",
    "__init__(self, secret: str)",
    "validate(token: str) -> bool",
    "fns:",
    "process(data: list) -> dict",
  })
end)
