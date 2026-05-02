local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has

case("csharp_all_sections", function()
  local src = [==[
using System;
using System.Collections.Generic;

namespace MyApp.Services;

public class UserService : BaseService, IDisposable
{
    private string _name;
    public UserService(string name) {}
    public void Dispose() {}
    public static string Format(int id) { return id.ToString(); }
}

public interface IRepository<T> : IEnumerable<T>
{
    T GetById(int id);
    void Save(T entity);
}

public enum Status
{
    Active,
    Inactive,
    Pending
}

public record Point(int X, int Y);

public struct Vector3 : IEquatable<Vector3>
{
    public float X;
    public float Y;
    public float Z;
}
]==]
  local out = idx(src, "c_sharp")
  has(out, {
    "imports:",
    "System",
    "System.Collections.Generic",
    "mod:",
    "MyApp.Services",
    "classes:",
    "public class UserService : BaseService, IDisposable",
    "private string _name",
    "public UserService(string name)",
    "public void Dispose()",
    "public static string Format(int id)",
    "traits:",
    "public interface IRepository",
    "T GetById(int id)",
    "void Save(T entity)",
    "types:",
    "public enum Status",
    "Active",
    "Inactive",
    "public record Point",
    "public struct Vector3",
  })
end)
