local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
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
