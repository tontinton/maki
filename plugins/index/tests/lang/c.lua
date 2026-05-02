-- C tests. Most cases here also exercise the C++ indexer to confirm shared
-- C-header constructs (typedefs, enums, includes, guards) work the same way
-- under both `c` and `cpp`. C++-specific syntax lives in cpp.lua.

local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has

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
