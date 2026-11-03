#include <iostream>
#include <vector>

namespace geo {

class Shape {
public:
    virtual ~Shape() = default;
    virtual double area() const = 0;
};

template <typename T>
struct Box {
    T value;
    T get() const { return value; }
};

class Circle : public Shape {
    double r_;
public:
    explicit Circle(double r) : r_(r) {}
    double area() const override { return 3.14159 * r_ * r_; }
};

}  // namespace geo

using Shapes = std::vector<geo::Shape*>;

int main() {
    geo::Circle c(2.0);
    std::cout << c.area() << std::endl;
    auto f = [](int x) { return x + 1; };
    return f(1) > 1 ? 0 : 1;
}
