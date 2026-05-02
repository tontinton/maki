local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
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
