import React, { useState } from "react";

interface Props {
  label: string;
}

export function Counter({ label }: Props) {
  const [count, setCount] = useState<number>(0);
  return (
    <button className="btn" onClick={() => setCount(count + 1)}>
      {label}: {count}
    </button>
  );
}

export const App = () => <Counter label="clicks" />;
