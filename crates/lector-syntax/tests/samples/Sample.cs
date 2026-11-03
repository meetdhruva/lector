using System;
using System.Collections.Generic;

namespace Demo.App
{
    public interface IShape
    {
        double Area();
    }

    public enum Color { Red, Green }

    public class Circle : IShape
    {
        private const double Pi = 3.14159;
        public double Radius { get; set; }

        public Circle(double radius)
        {
            Radius = radius;
        }

        public double Area() => Pi * Radius * Radius;
    }

    public static class Program
    {
        public static void Main(string[] args)
        {
            var shapes = new List<IShape> { new Circle(2.0) };
            foreach (var s in shapes)
            {
                Console.WriteLine($"area = {s.Area()}");
            }
        }
    }
}
