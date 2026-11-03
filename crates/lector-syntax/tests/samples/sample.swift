import Foundation

protocol Shape {
    func area() -> Double
}

struct Circle: Shape {
    var radius: Double
    func area() -> Double { return Double.pi * radius * radius }
}

enum Direction {
    case north, south
}

class Counter {
    private(set) var count = 0
    init() {}
    func increment() {
        count += 1
    }
}

extension Counter {
    func reset() { count = 0 }
}

typealias Callback = (Int) -> Void

let shapes: [Shape] = [Circle(radius: 1.0)]
for s in shapes {
    print("area: \(s.area())")
}
