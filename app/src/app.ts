import { App } from "aws-cdk-lib";
import { PatheryStack } from "@pathery/cdk";

const app = new App();

new PatheryStack(app, "pathery-dev");
