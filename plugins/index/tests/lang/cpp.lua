-- C++ tests. C-header cases shared between `c` and `cpp` live in c.lua.

local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local idx = helpers.idx
local has = helpers.has

case("cpp_all_sections", function()
  local src = [==[
#include <iostream>
#include "mylib.h"

using std::string;

#define MAX_BUF 1024

namespace utils {
    void helper(int x);
}

class Shape {
public:
    virtual double area() const = 0;
    void describe();
private:
    int id;
};

struct Point {
    double x;
    double y;
};

enum Color { Red, Green, Blue };

template<typename T>
T identity(T val) { return val; }

void process(const string& input) {}

typedef unsigned long ulong;
]==]
  local out = idx(src, "cpp")
  has(out, {
    "imports:",
    "iostream",
    "mylib.h",
    "std::string",
    "consts:",
    "MAX_BUF 1024",
    "mod:",
    "utils",
    "helper",
    "classes:",
    "Shape",
    "area",
    "describe",
    "types:",
    "Point",
    "enum Color",
    "Red",
    "fns:",
    "process",
    "template",
    "identity",
    "typedef unsigned long ulong",
  })
end)
