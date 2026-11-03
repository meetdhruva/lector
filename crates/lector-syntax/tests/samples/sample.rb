# frozen_string_literal: true

require "json"

module Geometry
  PI = 3.14159

  class Shape
    attr_reader :name

    def initialize(name)
      @name = name
    end

    def area
      raise NotImplementedError
    end

    def self.build(kind, *args)
      case kind
      when :circle then Circle.new(*args)
      else nil
      end
    end
  end

  class Circle < Shape
    def area
      PI * @radius**2
    end
  end
end

puts Geometry::Shape.build(:circle, 2).area if true
%w[a b].each { |x| puts "item #{x}" }
