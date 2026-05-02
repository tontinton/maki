local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has

case("go_all_sections", function()
  local src = [==[
package main

import (
	"fmt"
	"os"
)

const MaxRetries = 3

const (
	A = 1
	B = 2
)

var GlobalVar = "hello"

type Point struct {
	X int
	Y int
}

type Reader interface {
	Read(p []byte) (int, error)
}

type Alias = int

// Method doc
func (p *Point) Distance() float64 {
	return 0
}

func main() {
	fmt.Println("hello")
}
]==]
  local out = idx(src, "go")
  has(out, {
    "imports:",
    "fmt",
    "os",
    "consts:",
    "MaxRetries",
    "A",
    "B",
    "var GlobalVar",
    "types:",
    "struct Point",
    "X int",
    "Y int",
    "interface Reader",
    "Read(p []byte) (int, error)",
    "type Alias",
    "impls:",
    "(p *Point) Distance() float64",
    "fns:",
    "main()",
  })
end)
