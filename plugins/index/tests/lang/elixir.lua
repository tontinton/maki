local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local idx_with_meta = helpers.idx_with_meta
local has = helpers.has

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

case("elixir_module_methods_have_ranged_meta", function()
  local src = [==[
defmodule Calculator do
  def add(a, b) do
    a + b
  end

  defp validate(x) do
    x > 0
  end
end
]==]
  local text, meta = idx_with_meta(src, "elixir")
  helpers.assert_ranged_meta(text, meta, { "add", "validate" })
end)
