import pulumi
import pulumi_random as random

pet = random.RandomPet("demo")

pulumi.export("pet_name", pet.id)
