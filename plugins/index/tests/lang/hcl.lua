local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local idx = helpers.idx
local has = helpers.has
local lacks = helpers.lacks

case("hcl_blocks_with_string_labels", function()
  local src = [==[
resource "aws_instance" "web" {
  ami           = "ami-123"
  instance_type = "t3.micro"

  network_interface {
    device_index = 0
  }
}

variable "region" {
  default = "us-east-1"
}
]==]
  local out = idx(src, "hcl")
  has(out, {
    "blocks:",
    '  resource "aws_instance" "web" [1-8]',
    "    ami [2]",
    "    instance_type [3]",
    "    network_interface [5-7]",
    '  variable "region" [10-12]',
    "    default [11]",
  })
  lacks(out, {
    "ami-123",
    "t3.micro",
    "us-east-1",
    "device_index",
  })
end)

case("hcl_tfvars_top_level_attributes", function()
  local src = 'region = "us-east-1"\ninstance_count  = 3\n'
  local out = idx(src, "hcl")
  has(out, {
    "consts:",
    '  region = "us-east-1" [1]',
    "  instance_count = 3 [2]",
  })
end)

case("hcl_ranged_meta", function()
  local src = 'module "vpc" {\n  source = "./vpc"\n}\n'
  local out, meta = helpers.idx_with_meta(src, "hcl")
  helpers.assert_ranged_meta(out, meta, {
    'module "vpc"',
  })
end)
