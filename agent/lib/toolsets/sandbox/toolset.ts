import * as cdk from 'aws-cdk-lib';
import * as lambda from 'aws-cdk-lib/aws-lambda';
import * as dynamodb from 'aws-cdk-lib/aws-dynamodb';
import * as ec2 from 'aws-cdk-lib/aws-ec2';
import * as efs from 'aws-cdk-lib/aws-efs';
import * as path from 'path';

import { InfinityAgent } from '../../infinity-agents';
import { RapToolSet } from '../../infinity-agents/tools';

export interface SandboxToolSetProps {
  /**
   * VPC to place the Lambda and EFS in.
   * If not provided, a new VPC will be created.
   */
  readonly vpc?: ec2.IVpc;
}

/**
 * Sandboxed code editing and execution tools using jujutsu for filesystem versioning.
 * Deploys a container image Lambda with git + jj, backed by EFS for git repo storage
 * and DynamoDB for metadata.
 */
export class SandboxToolSet extends RapToolSet {
  public readonly metadataTable: dynamodb.Table;
  public readonly fileSystem: efs.FileSystem;
  public readonly vpc: ec2.IVpc;

  constructor(agent: InfinityAgent, id: string, props: SandboxToolSetProps = {}) {
    const vpc = props.vpc ?? new ec2.Vpc(agent, `${id}Vpc`, {
      maxAzs: 2,
      natGateways: 1,
    });

    const fileSystem = new efs.FileSystem(agent, `${id}FileSystem`, {
      vpc,
      removalPolicy: cdk.RemovalPolicy.RETAIN,
      performanceMode: efs.PerformanceMode.GENERAL_PURPOSE,
      throughputMode: efs.ThroughputMode.ELASTIC,
    });

    const accessPoint = fileSystem.addAccessPoint(`${id}AccessPoint`, {
      path: '/sandbox-repos',
      createAcl: { ownerGid: '1001', ownerUid: '1001', permissions: '755' },
      posixUser: { gid: '1001', uid: '1001' },
    });

    const metadataTable = new dynamodb.Table(agent, `${id}MetadataTable`, {
      partitionKey: { name: 'group_id', type: dynamodb.AttributeType.STRING },
      billingMode: dynamodb.BillingMode.PAY_PER_REQUEST,
      removalPolicy: cdk.RemovalPolicy.DESTROY,
    });

    // Container image Lambda with git + jj + the Rust binary
    const handler = new lambda.DockerImageFunction(agent, `${id}Function`, {
      code: lambda.DockerImageCode.fromImageAsset(
        path.join(__dirname, '../../../../'),
        {
          file: 'agent/lib/toolsets/sandbox/Dockerfile',
          platform: cdk.aws_ecr_assets.Platform.LINUX_ARM64,
          ignoreMode: cdk.IgnoreMode.GIT,
        },
      ),
      architecture: lambda.Architecture.ARM_64,
      timeout: cdk.Duration.minutes(5),
      memorySize: 512,
      recursiveLoop: lambda.RecursiveLoop.ALLOW,
      vpc,
      filesystem: lambda.FileSystem.fromEfsAccessPoint(accessPoint, '/mnt/efs'),
      environment: {
        EFS_MOUNT_PATH: '/mnt/efs/sandbox-repos',
        DYNAMODB_TABLE: metadataTable.tableName,
        RUST_BACKTRACE: '1',
      },
    });

    metadataTable.grantReadWriteData(handler);

    super(agent, id, { handler });

    this.metadataTable = metadataTable;
    this.fileSystem = fileSystem;
    this.vpc = vpc;
  }
}
