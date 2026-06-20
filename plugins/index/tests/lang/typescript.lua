local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local idx_with_meta = helpers.idx_with_meta
local has = helpers.has

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

case("ts_class_members_have_ranged_meta", function()
  local src = [==[export class Router {
    private routes: Map<string, Function>;
    add(path: string, handler: Function): void { this.routes.set(path, handler); }
    match(url: string): Function | null { return this.routes.get(url) || null; }
}
]==]
  local text, meta = idx_with_meta(src, "typescript")
  helpers.assert_ranged_meta(text, meta, { "add(", "match(" })
end)
