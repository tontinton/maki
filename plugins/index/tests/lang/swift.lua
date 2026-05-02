local helpers = require("tests.helpers")
local case = helpers.case
local idx = helpers.idx
local has = helpers.has

case("swift_all_sections", function()
  local src = [==[
import Foundation
import UIKit

public class Vehicle {
    public var name: String
    public init(name: String) {}
    public func start() {}
}

public struct Point {
    var x: Double
    var y: Double
}

public enum Direction {
    case north
    case south
}

public protocol Drawable {
    func draw()
}

extension Vehicle: Drawable {
    func draw() {}
}

public func process(input: String) -> Bool { return true }

let MAX_COUNT = 100
]==]
  local out = idx(src, "swift")
  has(out, {
    "imports:",
    "Foundation",
    "UIKit",
    "classes:",
    "Vehicle",
    "public var name",
    "public init",
    "public func start",
    "types:",
    "Point",
    "Direction",
    "traits:",
    "protocol Drawable",
    "func draw()",
    "impls:",
    "extension Vehicle",
    "fns:",
    "process",
  })
end)
