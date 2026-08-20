import pulumi
import pulumi_command as command
import pulumi_random as random

pet = random.RandomPet("demo")

hello = command.local.Command("hello", create="echo hello-from-pulumi2nix")

pulumi.export("pet_name", pet.id)
pulumi.export("hello", hello.stdout)
