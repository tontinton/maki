local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local idx_with_meta = helpers.idx_with_meta
local has = helpers.has

case("dart_class_with_methods", function()
  local src = [==[
class Person {
  String name;
  int age;

  Person(this.name, this.age);

  String greet() {
    return "Hello, $name";
  }

  int get yearsUntil100 => 100 - age;

  set incrementAge(int years) {
    age += years;
  }
}

String topLevelGreet(String name) {
  return "Hello, $name";
}

int get globalValue => 42;

set globalValue(int value) {
  // setter
}
]==]
  local out = idx(src, "dart")
  has(out, {
    "classes:",
    "class Person",
    "name String",
    "age int",
    "greet() String",
    "get yearsUntil100 int",
    "set incrementAge(int years)",
    "fns:",
    "topLevelGreet(String name) String",
    "get globalValue int",
    "set globalValue(int value)",
  })
end)

case("dart_class_methods_have_ranged_meta", function()
  local src = [==[
class Calculator {
  int add(int a, int b) {
    return a + b;
  }

  int subtract(int a, int b) {
    return a - b;
  }

  int multiply(int a, int b) {
    return a * b;
  }
}
]==]
  local text, meta = idx_with_meta(src, "dart")
  helpers.assert_ranged_meta(text, meta, { "add(int", "subtract(int", "multiply(int" })
end)

case("dart_named_constructors", function()
  local src = [==[
class Shape {
  Shape.unit();
  factory Shape.fromJson(Map json) => Shape._();
}
]==]
  local out = idx(src, "dart")
  has(out, {
    "classes:",
    "class Shape",
    "Shape.unit()",
    "Shape.fromJson(Map json)",
  })
end)

case("dart_generic_methods", function()
  local src = [==[
class A<T> {
  T transform<R>(R input) => input;
  Future<T> asyncMethod<T>() async => Future<T>.value();
}
]==]
  local out = idx(src, "dart")
  has(out, {
    "classes:",
    "class A<T>",
    "transform<R>(R input) T",
    "asyncMethod<T>() Future<T>",
  })
end)
