<?php
declare(strict_types=1);

namespace App\Models;

use InvalidArgumentException;

interface Shape
{
    public function area(): float;
}

trait Named
{
    public string $name = "shape";
}

abstract class Base implements Shape
{
    use Named;
    const VERSION = "1.0";
    protected int $count = 0;

    public function __construct(private float $size = 1.0)
    {
    }

    public static function create(float $size): static
    {
        if ($size < 0) {
            throw new InvalidArgumentException("negative");
        }
        return new static($size);
    }
}

function helper(array $items): int
{
    return count($items) + 1;
}
echo "done\n";
