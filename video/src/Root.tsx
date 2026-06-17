import "./index.css";
import { Composition } from "remotion";
import { TalesDemo } from "./Composition";

export const RemotionRoot: React.FC = () => {
  return (
    <>
      <Composition
        id="TalesDemo"
        component={TalesDemo}
        durationInFrames={810}
        fps={30}
        width={1920}
        height={1080}
      />
    </>
  );
};
