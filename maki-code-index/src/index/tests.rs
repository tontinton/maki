use std::path::Path;

use test_case::test_case;

use crate::{
    Language,
    index::{IndexError, MAX_FILE_SIZE, index_file, index_source},
};

fn idx(source: &str, lang: Language) -> String {
    index_source(source.as_bytes(), lang).unwrap()
}

fn has(output: &str, needles: &[&str]) {
    for n in needles {
        assert!(output.contains(n), "missing {n:?} in:\n{output}");
    }
}

fn lacks(output: &str, needles: &[&str]) {
    for n in needles {
        assert!(!output.contains(n), "unexpected {n:?} in:\n{output}");
    }
}

#[test]
fn unsupported_extension() {
    assert!(matches!(
        index_file(Path::new("file.yaml"), MAX_FILE_SIZE),
        Err(IndexError::UnsupportedLanguage(_))
    ));
}

#[test]
#[cfg(feature = "lang-rust")]
fn rust_all_sections() {
    let src = "\
//! Module doc
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
        write!(f, \"Foo\")
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
";
    let out = idx(src, Language::Rust);
    has(
        &out,
        &[
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
        ],
    );
    lacks(&out, &["{{"]);
}

#[test]
#[cfg(feature = "lang-rust")]
fn rust_many_fields_truncated() {
    let out = idx(
        "struct Big {\n    a: u8,\n    b: u8,\n    c: u8,\n    d: u8,\n    e: u8,\n    f: u8,\n    g: u8,\n    h: u8,\n    i: u8,\n    j: u8,\n}\n",
        Language::Rust,
    );
    has(&out, &["[2 more truncated]"]);
}

#[test]
#[cfg(feature = "lang-rust")]
fn rust_test_module_collapsed() {
    let src = "fn main() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn it_works() { assert!(true); }\n}\n";
    let out = idx(src, Language::Rust);
    has(&out, &["tests:"]);
    lacks(&out, &["it_works"]);
}

#[cfg(feature = "lang-rust")]
#[test_case("#[test]\nfn it_works() { assert!(true); }\n",         true  ; "standalone_test")]
#[test_case("#[tokio::test]\nasync fn my_test() {}\n",             true  ; "tokio_test")]
#[test_case("#[attested]\nfn foo() {}\n",                          false ; "attested_not_test")]
#[test_case("#[cfg(not(test))]\nfn real_fn() {}\n",                false ; "cfg_not_test")]
#[test_case("#[my_crate::test_helper]\nfn setup() {}\n",           false ; "test_helper_not_test")]
fn rust_test_detection(src: &str, is_test: bool) {
    let out = idx(src, Language::Rust);
    if is_test {
        has(&out, &["tests:"]);
        lacks(&out, &["fns:"]);
    } else {
        has(&out, &["fns:"]);
        lacks(&out, &["tests:"]);
    }
}

#[cfg(feature = "lang-rust")]
#[test_case(
    "/// Documented\n/// More docs\npub fn foo() {}\n",
    "pub foo() [1-3]"
    ; "doc_comment_extends_range"
)]
#[test_case(
    "/// Doc\n#[derive(Debug)]\npub struct Bar {\n    pub x: i32,\n}\n",
    "pub struct Bar [1-5]"
    ; "doc_plus_attr_extends_range"
)]
#[test_case(
    "pub fn plain() {}\n",
    "pub plain() [1]"
    ; "no_doc_comment"
)]
#[test_case(
    "// regular comment\npub fn foo() {}\n",
    "pub foo() [2]"
    ; "regular_comment_not_doc"
)]
fn rust_doc_comment_line_ranges(src: &str, expected: &str) {
    let out = idx(src, Language::Rust);
    has(&out, &[expected]);
}

#[test]
#[cfg(feature = "lang-python")]
fn python_all_sections() {
    let src = "\
\"\"\"Module docstring.\"\"\"

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
";
    let out = idx(src, Language::Python);
    has(
        &out,
        &[
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
        ],
    );
    lacks(&out, &["MY_VAR = int"]);
}

#[test]
#[cfg(feature = "lang-typescript")]
fn ts_all_sections() {
    let src = "\
/** Function docs */
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
";
    let out = idx(src, Language::TypeScript);
    has(
        &out,
        &[
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
        ],
    );
}

#[test]
#[cfg(feature = "lang-go")]
fn go_all_sections() {
    let src = r#"
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
"#;
    let out = idx(src, Language::Go);
    has(
        &out,
        &[
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
            "type Alias",
            "traits:",
            "Reader",
            "Read(p []byte) (int, error)",
            "impls:",
            "(p *Point) Distance() float64",
            "fns:",
            "main()",
        ],
    );
    lacks(&out, &["package"]);
}

#[test]
#[cfg(feature = "lang-java")]
fn java_all_sections() {
    let src = r#"
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
"#;
    let out = idx(src, Language::Java);
    has(
        &out,
        &[
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
        ],
    );
}

#[test]
#[cfg(feature = "lang-ruby")]
fn ruby_all_sections() {
    let src = r#"
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
"#;
    let out = idx(src, Language::Ruby);
    has(
        &out,
        &[
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
        ],
    );
    lacks(&out, &["end"]);
}

#[test]
#[cfg(feature = "lang-c")]
fn c_all_sections() {
    let src = r#"
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
"#;
    let out = idx(src, Language::C);
    has(
        &out,
        &[
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
        ],
    );
    lacks(&out, &["return 0"]);
}

#[test]
#[cfg(feature = "lang-c-sharp")]
fn csharp_all_sections() {
    let src = r#"
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
"#;
    let out = idx(src, Language::CSharp);
    has(
        &out,
        &[
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
        ],
    );
    lacks(&out, &["return"]);
}

#[test]
#[cfg(feature = "lang-lua")]
fn lua_all_sections() {
    let src = r#"
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
"#;
    let out = idx(src, Language::Lua);
    has(
        &out,
        &[
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
        ],
    );
    lacks(&out, &["min_val"]);
}

#[test]
#[cfg(feature = "lang-cpp")]
fn cpp_all_sections() {
    let src = r#"
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
"#;
    let out = idx(src, Language::Cpp);
    has(
        &out,
        &[
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
        ],
    );
    lacks(&out, &["return"]);
}

#[test]
#[cfg(feature = "lang-php")]
fn php_all_sections() {
    let src = r#"<?php
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
"#;
    let out = idx(src, Language::Php);
    has(
        &out,
        &[
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
        ],
    );
    lacks(&out, &["return"]);
}

#[test]
#[cfg(feature = "lang-swift")]
fn swift_all_sections() {
    let src = r#"
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
"#;
    let out = idx(src, Language::Swift);
    has(
        &out,
        &[
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
        ],
    );
    lacks(&out, &["return true"]);
}

#[test]
#[cfg(feature = "lang-scala")]
fn scala_all_sections() {
    let src = r#"
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
"#;
    let out = idx(src, Language::Scala);
    has(
        &out,
        &[
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
        ],
    );
    lacks(&out, &["true", "???", "x.toString"]);
}

#[test]
#[cfg(feature = "lang-bash")]
fn bash_all_sections() {
    let src = r#"
#!/bin/bash

MAX_RETRIES=5
LOG_DIR="/var/log"

my_func() {
    echo "hello"
}

function process() {
    echo "processing"
}
"#;
    let out = idx(src, Language::Bash);
    has(
        &out,
        &[
            "consts:",
            "MAX_RETRIES = 5",
            "LOG_DIR",
            "fns:",
            "my_func()",
            "process()",
        ],
    );
    lacks(&out, &["echo"]);
}

#[test]
#[cfg(feature = "lang-kotlin")]
fn kotlin_all_sections() {
    let src = r#"
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
"#;
    let out = idx(src, Language::Kotlin);
    has(
        &out,
        &[
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
        ],
    );
}

#[test]
#[cfg(feature = "lang-elixir")]
fn elixir_all_sections() {
    let src = r#"
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
"#;
    let out = idx(src, Language::Elixir);
    has(
        &out,
        &[
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
        ],
    );
    lacks(&out, &[":ok", "true", "{:ok, state}"]);
}
