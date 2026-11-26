local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local indexer = require("indexer")
local case = th.case
local eq = th.eq
local idx = helpers.idx
local has = helpers.has

local TSX_LANG = "tsx"
local JS_LANG = "javascript"
local BROKEN_SOURCE = "}}} not code {{{\n"

-- #999: on the plain TypeScript grammar these all landed in an ERROR node, so
-- index printed nothing at all or text sliced out of the middle of a tag.
case("tsx_jsx_value_collapses_to_the_rendered_tag", function()
  has(idx("export const x = <T a={<B c={d} />} />;", TSX_LANG), { "consts:", "export x = <T />" })
  has(idx("export const x = <T>{<B c={d} />}</T>;", TSX_LANG), { "export x = <T>...</T>" })
  has(idx("export const x = <><B c={d} /></>;", TSX_LANG), { "export x = <>...</>" })
end)

case("tsx_jsx_in_javascript_sources", function()
  has(idx("export const x = <T a={<B c={d} />} />;", JS_LANG), { "export x = <T />" })
end)

case("tsx_keeps_typescript_constructs", function()
  local src = [==[import { useState } from 'react';

export interface Props {
    title: string;
}

export type ID = string | number;

export class Widget {
    render(): Element { return <div />; }
}

export function App({ title }: Props) {
    const [n] = useState(0);
    return <main>{title}{n}</main>;
}
]==]
  has(idx(src, TSX_LANG), {
    "imports:",
    "{ useState } from 'react'",
    "types:",
    "export interface Props",
    "title: string",
    "type ID",
    "classes:",
    "export Widget",
    "render()",
    "fns:",
    "export App({ title }: Props)",
  })
end)

case("tsx_unparsable_source_reports_instead_of_returning_nothing", function()
  eq(idx(BROKEN_SOURCE, TSX_LANG), indexer.PARSE_ERROR_NOTE .. "\n")
end)
