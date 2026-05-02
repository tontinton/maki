local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has

case("php_all_sections", function()
  local src = [==[<?php
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
]==]
  local out = idx(src, "php")
  has(out, {
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
  })
end)
