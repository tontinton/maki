local indexer = require("indexer")

local failures = {}

local function case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    table.insert(failures, name .. ": " .. tostring(err))
  end
end

local function idx(source, lang)
  local result, err = indexer.index_source(source, lang)
  assert(result, "index failed for " .. lang .. ": " .. tostring(err))
  return result
end

local function has(output, needles)
  for _, n in ipairs(needles) do
    assert(output:find(n, 1, true), "missing '" .. n .. "'")
  end
end

local function lacks(output, needles)
  for _, n in ipairs(needles) do
    assert(not output:find(n, 1, true), "unexpected '" .. n .. "'")
  end
end

case("rust_all_sections", function()
  local src = [[//! Module doc
use std::collections::HashMap;
use std::io;
use std::io::*;
use std::{fs, net};

const MAX: usize = 1024;
static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub port: u16,
}

pub struct Empty;

enum Color { Red, Green }

pub type Result<T> = std::result::Result<T, MyError>;

pub trait Handler {
    fn handle(&self, req: Request) -> Response;
}

impl Display for Foo {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "Foo")
    }
}

impl Config {
    pub fn new(name: String) -> Self { todo!() }
    fn validate(&self) -> bool { true }
}

pub fn process(input: &str) -> Result<String, Error> { todo!() }

pub mod utils;
mod internal;

macro_rules! my_macro { () => {}; }
]]
  local out = idx(src, "rust")
  has(out, {
    "module doc:",
    "imports:",
    "std::",
    "collections::HashMap",
    "io",
    "io::*",
    "fs",
    "net",
    "consts:",
    "MAX: usize",
    "static COUNTER: AtomicU64",
    "types:",
    "#[derive(Debug, Clone)]",
    "pub struct Config",
    "pub name: String",
    "pub struct Empty",
    "enum Color",
    "Red, Green",
    "type Result",
    "traits:",
    "pub Handler",
    "handle(&self, req: Request) -> Response",
    "impls:",
    "Display for Foo",
    "Config",
    "pub new(name: String) -> Self",
    "validate(&self) -> bool",
    "fns:",
    "pub process(input: &str)",
    "mod:",
    "pub utils, internal",
    "macros:",
    "my_macro!",
  })
end)

case("rust_many_fields_truncated", function()
  local out = idx(
    "struct Big {\n    a: u8,\n    b: u8,\n    c: u8,\n    d: u8,\n    e: u8,\n    f: u8,\n    g: u8,\n    h: u8,\n    i: u8,\n    j: u8,\n}\n",
    "rust"
  )
  has(out, { "[2 more truncated]" })
end)

case("rust_test_module_collapsed", function()
  local src =
    "fn main() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn it_works() { assert!(true); }\n}\n"
  local out = idx(src, "rust")
  has(out, { "tests:" })
  lacks(out, { "it_works" })
end)

case("rust_test_detection", function()
  local cases = {
    { src = "#[test]\nfn it_works() { assert!(true); }\n", test = true, name = "standalone_test" },
    { src = "#[tokio::test]\nasync fn my_test() {}\n", test = true, name = "tokio_test" },
    { src = "#[attested]\nfn foo() {}\n", test = false, name = "attested_not_test" },
    { src = "#[cfg(not(test))]\nfn real_fn() {}\n", test = false, name = "cfg_not_test" },
    { src = "#[my_crate::test_helper]\nfn setup() {}\n", test = false, name = "test_helper_not_test" },
  }
  for _, c in ipairs(cases) do
    local out = idx(c.src, "rust")
    if c.test then
      has(out, { "tests:" })
      lacks(out, { "fns:" })
    else
      has(out, { "fns:" })
      lacks(out, { "tests:" })
    end
  end
end)

case("rust_doc_comment_line_ranges", function()
  local cases = {
    { src = "/// Documented\n/// More docs\npub fn foo() {}\n", expected = "pub foo() [1-3]" },
    {
      src = "/// Doc\n#[derive(Debug)]\npub struct Bar {\n    pub x: i32,\n}\n",
      expected = "pub struct Bar [1-5]",
    },
    { src = "pub fn plain() {}\n", expected = "pub plain() [1]" },
    { src = "// regular comment\npub fn foo() {}\n", expected = "pub foo() [2]" },
  }
  for _, c in ipairs(cases) do
    local out = idx(c.src, "rust")
    has(out, { c.expected })
  end
end)

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

case("ts_all_sections", function()
  local src = [==[/** Function docs */
import { Request, Response } from 'express';

export interface Config {
    port: number;
    host: string;
}

export type ID = string | number;

export enum Direction { Up, Down }

export const PORT: number = 3000;

export class Service {
    process(input: string): string { return input; }
}

/** Handler doc */
export function handler(req: Request): Response { return new Response(); }
]==]
  local out = idx(src, "typescript")
  has(out, {
    "imports:",
    "{ Request, Response } from 'express'",
    "types:",
    "export interface Config",
    "port: number",
    "type ID",
    "export enum Direction",
    "consts:",
    "PORT",
    "classes:",
    "export Service",
    "fns:",
    "export handler(req: Request)",
  })
end)

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

case("c_all_sections", function()
  local src = [==[
/** Module header */
#include <stdio.h>
#include "my_lib.h"

#define MAX_SIZE 256
#define VERSION "1.0"

typedef struct {
    int x;
    int y;
} Point;

typedef enum {
    RED,
    GREEN,
    BLUE,
} Color;

typedef unsigned int uint32;

struct Node {
    int value;
    struct Node *next;
};

enum Direction {
    UP,
    DOWN,
};

/** Add two numbers */
int add(int a, int b);

void process(const char *input, size_t len);

int main(int argc, char **argv) {
    return 0;
}
]==]
  for _, lang in ipairs({ "c", "cpp" }) do
    local out = idx(src, lang)
    has(out, {
      "imports:",
      "stdio.h",
      "my_lib.h",
      "consts:",
      "MAX_SIZE 256",
      "VERSION",
      "types:",
      "typedef struct",
      "int x",
      "int y",
      "typedef enum",
      "RED",
      "GREEN",
      "typedef unsigned int uint32",
      "struct Node",
      "enum Direction",
      "UP",
      "fns:",
      "int add(int a, int b)",
      "void process(const char *input, size_t len)",
      "int main(int argc, char **argv)",
    })
  end
end)

case("c_extern_c_wrapped", function()
  local src = [==[
#include <glib.h>

G_BEGIN_DECLS

#define MAX_SIZE 256

typedef enum {
    RED,
    GREEN,
    BLUE,
} Color;

typedef struct {
    int x;
    int y;
} Point;

int add(int a, int b);
void process(const char *input, size_t len);

G_END_DECLS
]==]
  for _, lang in ipairs({ "c", "cpp" }) do
    local out = idx(src, lang)
    has(out, {
      "imports:",
      "glib.h",
      "consts:",
      "MAX_SIZE 256",
      "types:",
      "enum",
      "RED",
      "GREEN",
      "BLUE",
      "struct",
      "int x",
      "int y",
      "fns:",
      "int add(int a, int b)",
      "void process(const char *input, size_t len)",
    })
  end
end)

case("c_extern_c", function()
  local src = [==[
#include <stdio.h>

#ifdef __cplusplus
extern "C" {
#endif

#define MAX_SIZE 256

typedef enum {
    RED,
    GREEN,
    BLUE,
} Color;

typedef struct {
    int x;
    int y;
} Point;

int add(int a, int b);
void process(const char *input, size_t len);

#ifdef __cplusplus
}
#endif
]==]
  for _, lang in ipairs({ "c", "cpp" }) do
    local out = idx(src, lang)
    has(out, {
      "imports:",
      "stdio.h",
      "consts:",
      "MAX_SIZE 256",
      "types:",
      "enum",
      "RED",
      "GREEN",
      "BLUE",
      "struct",
      "int x",
      "int y",
      "fns:",
      "int add(int a, int b)",
      "void process(const char *input, size_t len)",
    })
  end
end)

case("c_single_include_guards", function()
  local src = [==[
#ifndef __MY_HEADER_H__
#define __MY_HEADER_H__
#include <stdio.h>

#define MAX_SIZE 256

typedef enum {
    RED,
    GREEN,
    BLUE,
} Color;

typedef struct {
    int x;
    int y;
} Point;

int add(int a, int b);
void process(const char *input, size_t len);

#endif /* __MY_HEADER_H__ */
]==]
  for _, lang in ipairs({ "c", "cpp" }) do
    local out = idx(src, lang)
    has(out, {
      "imports:",
      "stdio.h",
      "consts:",
      "MAX_SIZE 256",
      "types:",
      "enum",
      "RED",
      "GREEN",
      "BLUE",
      "struct",
      "int x",
      "int y",
      "fns:",
      "int add(int a, int b)",
      "void process(const char *input, size_t len)",
    })
  end
end)

case("c_single_include_pragma", function()
  local src = [==[
#pragma "once"
#include <stdio.h>

#define MAX_SIZE 256

typedef enum {
    RED,
    GREEN,
    BLUE,
} Color;

typedef struct {
    int x;
    int y;
} Point;

int add(int a, int b);
void process(const char *input, size_t len);
]==]
  for _, lang in ipairs({ "c", "cpp" }) do
    local out = idx(src, lang)
    has(out, {
      "imports:",
      "stdio.h",
      "consts:",
      "MAX_SIZE 256",
      "types:",
      "enum",
      "RED",
      "GREEN",
      "BLUE",
      "struct",
      "int x",
      "int y",
      "fns:",
      "int add(int a, int b)",
      "void process(const char *input, size_t len)",
    })
  end
end)

case("csharp_all_sections", function()
  local src = [==[
using System;
using System.Collections.Generic;

namespace MyApp.Services;

public class UserService : BaseService, IDisposable
{
    private string _name;
    public UserService(string name) {}
    public void Dispose() {}
    public static string Format(int id) { return id.ToString(); }
}

public interface IRepository<T> : IEnumerable<T>
{
    T GetById(int id);
    void Save(T entity);
}

public enum Status
{
    Active,
    Inactive,
    Pending
}

public record Point(int X, int Y);

public struct Vector3 : IEquatable<Vector3>
{
    public float X;
    public float Y;
    public float Z;
}
]==]
  local out = idx(src, "c_sharp")
  has(out, {
    "imports:",
    "System",
    "System.Collections.Generic",
    "mod:",
    "MyApp.Services",
    "classes:",
    "public class UserService : BaseService, IDisposable",
    "private string _name",
    "public UserService(string name)",
    "public void Dispose()",
    "public static string Format(int id)",
    "traits:",
    "public interface IRepository",
    "T GetById(int id)",
    "void Save(T entity)",
    "types:",
    "public enum Status",
    "Active",
    "Inactive",
    "public record Point",
    "public struct Vector3",
  })
end)

case("lua_all_sections", function()
  local src = [==[
local json = require("cjson")
local x, y = require("foo"), require("bar")
require("init")

local MAX_SIZE = 100
local min_val = 10

function process(data, opts)
  return data
end

function M.helper(x)
end

function M:method(self, val)
end
]==]
  local out = idx(src, "lua_lang")
  has(out, {
    "imports:",
    "cjson",
    "foo",
    "bar",
    "init",
    "consts:",
    "MAX_SIZE = 100",
    "fns:",
    "process(data, opts)",
    "M.helper(x)",
    "M:method(self, val)",
  })
end)

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

case("php_all_sections", function()
  local src = [==[<?php
namespace App\Services;

use App\Models\User;

const VERSION = "1.0";

class UserService extends BaseService implements Serializable
{
    private string $name;
    public function __construct(string $name) {}
    public function find(int $id): ?User {}
    public static function create(array $data): self {}
}

interface Repository
{
    public function getById(int $id): mixed;
    public function save(object $entity): void;
}

trait Loggable
{
}

function helper(string $input): string {}

enum Status
{
    case Active;
    case Inactive;
}
]==]
  local out = idx(src, "php")
  has(out, {
    "mod:",
    "App\\Services",
    "imports:",
    "App\\Models\\User",
    "consts:",
    "VERSION",
    "classes:",
    "UserService extends BaseService implements Serializable",
    "public function __construct(string $name)",
    "public function find(int $id): ?User",
    "public static function create(array $data): self",
    "traits:",
    "Repository",
    "Loggable",
    "fns:",
    "helper(string $input): string",
    "types:",
    "enum Status",
  })
end)

case("swift_all_sections", function()
  local src = [==[
import Foundation
import UIKit

public class Vehicle {
    public var name: String
    public init(name: String) {}
    public func start() {}
}

public struct Point {
    var x: Double
    var y: Double
}

public enum Direction {
    case north
    case south
}

public protocol Drawable {
    func draw()
}

extension Vehicle: Drawable {
    func draw() {}
}

public func process(input: String) -> Bool { return true }

let MAX_COUNT = 100
]==]
  local out = idx(src, "swift")
  has(out, {
    "imports:",
    "Foundation",
    "UIKit",
    "classes:",
    "Vehicle",
    "public var name",
    "public init",
    "public func start",
    "types:",
    "Point",
    "Direction",
    "traits:",
    "protocol Drawable",
    "func draw()",
    "impls:",
    "extension Vehicle",
    "fns:",
    "process",
  })
end)

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

case("bash_all_sections", function()
  local src = [==[
#!/bin/bash

MAX_RETRIES=5
LOG_DIR="/var/log"

my_func() {
    echo "hello"
}

function process() {
    echo "processing"
}
]==]
  local out = idx(src, "bash")
  has(out, {
    "consts:",
    "MAX_RETRIES = 5",
    "LOG_DIR",
    "fns:",
    "my_func()",
    "process()",
  })
end)

case("kotlin_all_sections", function()
  local src = [==[
package com.example

import kotlin.collections.List
import java.io.File

const val MAX_SIZE = 100
val SOME_CONST = "value"

typealias StringList = List<String>

data class User(val name: String, val age: Int) : Comparable<User> {
    fun greet(): String = "Hello $name"
}

object Singleton {
    fun instance(): Singleton = this
}

fun topLevel(x: Int): Int = x

enum class Color {
    RED, GREEN, BLUE
}
]==]
  local out = idx(src, "kotlin")
  has(out, {
    "mod:",
    "com.example",
    "imports:",
    "kotlin.collections.List",
    "consts:",
    "MAX_SIZE",
    "types:",
    "StringList",
    "classes:",
    "User",
    "greet",
    "Singleton",
    "instance",
    "Color",
    "fns:",
    "topLevel",
  })
end)

case("elixir_all_sections", function()
  local src = [==[
defmodule MyApp.Web do
  alias Phoenix.Controller
  import Plug.Conn
  use MyApp.Web, :controller
  require Logger

  @doc "Process data"
  def process(conn, params) do
    :ok
  end

  defp validate(data) do
    true
  end
end

defmodule MyApp.Helpers do
  def format_name(name) do
    name
  end
end

@MAX_RETRIES 3

def handle_event(event, state) do
  {:ok, state}
end
]==]
  local out = idx(src, "elixir")
  has(out, {
    "imports:",
    "Phoenix.Controller",
    "Plug.Conn",
    "use: MyApp.Web",
    "require: Logger",
    "classes:",
    "defmodule MyApp.Web",
    "process(conn, params)",
    "validate(data)",
    "defmodule MyApp.Helpers",
    "format_name(name)",
    "consts:",
    "@MAX_RETRIES",
    "fns:",
    "handle_event(event, state)",
  })
end)

case("markdown_atx_headings", function()
  local src =
    "# Main Title\n\n## Section 1\n\nSome text here.\n\n### Subsection 1.1\n\nMore content.\n\n## Section 2\n\n### Another Subsection 2.1\n\n#### Deep Heading 2.1.3\n\n# Footer Title\n\nAnd some more content\n"
  local out = idx(src, "markdown")
  has(out, {
    "headings:",
    "# Main Title [1-16]",
    "## Section 1 [3-10]",
    "### Subsection 1.1 [7-10]",
    "## Section 2 [11-16]",
    "### Another Subsection 2.1 [13-16]",
    "#### Deep Heading 2.1.3 [15-16]",
    "# Footer Title [17-20]",
  })
  lacks(out, { "Some text here", "More content", "And some more content" })
end)

case("markdown_atx_headings_no_newline", function()
  local src = "# Main Title\nSome text here\n# Footer Title"
  local out = idx(src, "markdown")
  has(out, { "headings:", "# Main Title [1-2]", "# Footer Title [3]" })
  lacks(out, { "Some text here" })
end)

case("markdown_setext_headings", function()
  local src =
    "Heading 1\n=========\n\nSome text here\n\nHeading 1.1\n---------\n\nMore content\n\nHeading 2\n=========\n\nAnd some more content\n"
  local out = idx(src, "markdown")
  has(out, {
    "headings:",
    "# Heading 1 [1-10]",
    "## Heading 1.1 [6-10]",
    "# Heading 2 [11-15]",
  })
  lacks(out, { "Some text here", "More content", "And some more content" })
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
