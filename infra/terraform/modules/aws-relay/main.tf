# A PingPath relay on one EC2 instance with a fixed public address. The
# address is allocated separately from the instance, so relays can list each
# other as peers without a dependency cycle.

module "node" {
  source          = "../relay-node"
  relay           = merge(var.relay, { id = var.id, name = var.name, port = var.port })
  control_api_key = var.control_api_key
}

locals {
  tags = merge({ Name = var.name, "pingpath:relay-id" = tostring(var.id) }, var.tags)
}

data "aws_ami" "ubuntu" {
  count       = var.ami_id == "" ? 1 : 0
  most_recent = true
  owners      = ["099720109477"] # Canonical
  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*"]
  }
  filter {
    name   = "architecture"
    values = ["x86_64"]
  }
}

data "aws_vpc" "default" {
  count   = var.subnet_id == "" ? 1 : 0
  default = true
}

data "aws_subnets" "default" {
  count = var.subnet_id == "" ? 1 : 0
  filter {
    name   = "vpc-id"
    values = [data.aws_vpc.default[0].id]
  }
  filter {
    name   = "default-for-az"
    values = ["true"]
  }
}

data "aws_subnet" "chosen" {
  id = var.subnet_id != "" ? var.subnet_id : sort(data.aws_subnets.default[0].ids)[0]
}

resource "aws_security_group" "relay" {
  name_prefix = "pingpath-${var.name}-"
  description = "PingPath relay ${var.name}: UDP for clients and relays; SSH and metrics restricted"
  vpc_id      = data.aws_subnet.chosen.vpc_id
  tags        = local.tags

  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_vpc_security_group_ingress_rule" "udp_v4" {
  security_group_id = aws_security_group.relay.id
  description       = "Clients and other relays"
  ip_protocol       = "udp"
  from_port         = var.port
  to_port           = var.port
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_vpc_security_group_ingress_rule" "udp_v6" {
  security_group_id = aws_security_group.relay.id
  description       = "Clients and other relays"
  ip_protocol       = "udp"
  from_port         = var.port
  to_port           = var.port
  cidr_ipv6         = "::/0"
}

resource "aws_vpc_security_group_ingress_rule" "ssh" {
  for_each          = toset(var.admin_cidrs)
  security_group_id = aws_security_group.relay.id
  description       = "SSH from an admin address"
  ip_protocol       = "tcp"
  from_port         = 22
  to_port           = 22
  cidr_ipv4         = each.value
}

resource "aws_vpc_security_group_ingress_rule" "metrics" {
  for_each          = toset(var.monitoring_cidrs)
  security_group_id = aws_security_group.relay.id
  description       = "Health and metrics from monitoring"
  ip_protocol       = "tcp"
  from_port         = 9100
  to_port           = 9100
  cidr_ipv4         = each.value
}

resource "aws_vpc_security_group_egress_rule" "all_v4" {
  security_group_id = aws_security_group.relay.id
  description       = "Game servers, other relays, the control plane, package mirrors"
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
}

resource "aws_vpc_security_group_egress_rule" "all_v6" {
  security_group_id = aws_security_group.relay.id
  description       = "Game servers, other relays, the control plane, package mirrors"
  ip_protocol       = "-1"
  cidr_ipv6         = "::/0"
}

resource "aws_eip" "relay" {
  domain = "vpc"
  tags   = local.tags
}

resource "aws_instance" "relay" {
  ami                    = var.ami_id != "" ? var.ami_id : data.aws_ami.ubuntu[0].id
  instance_type          = var.instance_type
  subnet_id              = data.aws_subnet.chosen.id
  vpc_security_group_ids = [aws_security_group.relay.id]
  key_name               = var.ssh_key_name != "" ? var.ssh_key_name : null
  user_data              = module.node.user_data
  # A config change replaces the instance; the exit gets a new WireGuard
  # key, so update the control plane's registry afterwards.
  user_data_replace_on_change = true

  metadata_options {
    http_tokens   = "required" # IMDSv2 only
    http_endpoint = "enabled"
  }

  root_block_device {
    volume_type = "gp3"
    volume_size = 10
    encrypted   = true
  }

  tags = local.tags
}

resource "aws_eip_association" "relay" {
  allocation_id = aws_eip.relay.id
  instance_id   = aws_instance.relay.id
}
