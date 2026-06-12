local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

case("html_structure", function()
  local src = [==[
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Test</title>
  <style>body { margin: 0; }</style>
</head>
<body>
  <header><nav><ul><li>item</li></ul></nav></header>
  <main><section><div><p>text</p></div></section></main>
  <footer><p>Footer</p></footer>
  <script>console.log("hi");</script>
</body>
</html>
]==]
  local out = idx(src, "html")
  has(out, {
    "structure:",
    "<html>",
    "<head>",
    "<body>",
    "<nav>",
    "<main>",
    "<div>",
    "<footer>",
    "<style>",
    "<script>",
  })
end)

case("html_attributes", function()
  local src = [==[
<html>
<body>
  <div id="myId" class="cls1 cls2">
    <section class="intro">
      <p>Text</p>
    </section>
  </div>
</body>
</html>
]==]
  local out = idx(src, "html")
  has(out, {
    "<div#myId.cls1.cls2>",
    "<section.intro>",
  })
end)

case("html_filtering", function()
  local src = [==[
<html>
<head>
  <meta id="viewport" name="viewport" />
  <meta charset="utf-8" />
</head>
<body>
  <span>text</span>
  <p>paragraph</p>
  <a href="#">link</a>
  <strong>bold</strong>
  <span id="promoted">kept</span>
  <p id="bar" class="cls">also kept</p>
  <div>structural</div>
</body>
</html>
]==]
  local out = idx(src, "html")
  has(out, {
    "<meta#viewport>",
    "<span#promoted>",
    "<p#bar.cls>",
    "<div>",
  })
  lacks(out, { "<span>", "<p>", "<a>", "<strong>" })
end)
